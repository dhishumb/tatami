//! Thin wrappers over the Accessibility C API. Every call here is one IPC
//! round trip to the target app (except `window_id` and `pid`), so callers
//! batch reads with `attrs` where they can.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions,
    AXUIElement, AXValue, AXValueType, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType, CGPoint, CGSize, Type,
};
use tatami_core::PlatformError;

pub type AxResult<T> = Result<T, AXError>;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Private but stable since 10.x; every macOS window manager relies on it.
    #[link_name = "_AXUIElementGetWindow"]
    fn ax_ui_element_get_window(element: &AXUIElement, out: *mut u32) -> AXError;
}

/// Whether this process may use the Accessibility API.
pub fn is_trusted() -> bool {
    // SAFETY: no arguments, no preconditions.
    unsafe { AXIsProcessTrusted() }
}

/// Like `is_trusted`, but asks macOS to show its permission prompt when untrusted.
pub fn prompt_trust() -> bool {
    // SAFETY: reading an immutable framework constant.
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let opts = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);
    // SAFETY: `opts` is a valid CFDictionary of CFString -> CFBoolean, as the API expects.
    unsafe { AXIsProcessTrustedWithOptions(Some(opts.as_ref())) }
}

pub fn to_platform_error(e: AXError) -> PlatformError {
    if e == AXError::APIDisabled {
        PlatformError::NotTrusted
    } else {
        PlatformError::Ax(e.0)
    }
}

pub(crate) fn check(e: AXError) -> AxResult<()> {
    if e == AXError::Success {
        Ok(())
    } else {
        Err(e)
    }
}

/// A retained `AXUIElementRef` (an app, a window, a button...).
#[derive(Clone)]
pub struct AxElement(CFRetained<AXUIElement>);

impl AxElement {
    pub fn application(pid: i32) -> Self {
        // SAFETY: any pid is accepted; a dead pid yields an element whose calls fail.
        Self(unsafe { AXUIElement::new_application(pid) })
    }

    pub fn system_wide() -> Self {
        // SAFETY: no arguments, no preconditions.
        Self(unsafe { AXUIElement::new_system_wide() })
    }

    /// Retain an element borrowed from a callback.
    pub fn from_ref(el: &AXUIElement) -> Self {
        Self(el.retain())
    }

    /// The underlying element, for `AXObserver` registration.
    pub fn raw(&self) -> &AXUIElement {
        &self.0
    }

    /// Bound how long any AX call on this element may block. On the
    /// system-wide element it sets the default for the whole process.
    pub fn set_timeout(&self, secs: f32) -> AxResult<()> {
        // SAFETY: plain value argument.
        check(unsafe { self.0.set_messaging_timeout(secs) })
    }

    pub fn attr(&self, name: &'static str) -> AxResult<CFRetained<CFType>> {
        let name = CFString::from_static_str(name);
        let mut out: *const CFType = ptr::null();
        // SAFETY: `out` is a valid out-pointer; on success it holds a +1 reference.
        check(unsafe { self.0.copy_attribute_value(&name, NonNull::from(&mut out)) })?;
        let out = NonNull::new(out.cast_mut()).ok_or(AXError::NoValue)?;
        // SAFETY: Copy rule: we own the reference returned above.
        Ok(unsafe { CFRetained::from_raw(out) })
    }

    /// Fetch several attributes in one round trip. A slot is `None` when that
    /// attribute errored (the API returns an AXError-typed AXValue there).
    pub fn attrs(&self, names: &CFArray<CFString>) -> AxResult<Vec<Option<CFRetained<CFType>>>> {
        let mut out: *const CFArray = ptr::null();
        // SAFETY: `names` is an array of CFStrings; `out` is a valid out-pointer that receives a +1 array.
        check(unsafe {
            self.0.copy_multiple_attribute_values(
                names.as_ref(),
                AXCopyMultipleAttributeOptions::empty(),
                NonNull::from(&mut out),
            )
        })?;
        let out = NonNull::new(out.cast_mut()).ok_or(AXError::NoValue)?;
        // SAFETY: Copy rule; every element of a CFArray is a CFType.
        let arr: CFRetained<CFArray<CFType>> = unsafe { CFRetained::from_raw(out.cast()) };
        Ok(arr
            .iter()
            .map(|v| if is_ax_error(&v) { None } else { Some(v) })
            .collect())
    }

    pub fn attr_string(&self, name: &'static str) -> AxResult<String> {
        as_string(&*self.attr(name)?).ok_or(AXError::IllegalArgument)
    }

    pub fn attr_bool(&self, name: &'static str) -> AxResult<bool> {
        as_bool(&*self.attr(name)?).ok_or(AXError::IllegalArgument)
    }

    pub fn attr_element(&self, name: &'static str) -> AxResult<AxElement> {
        as_element(self.attr(name)?).ok_or(AXError::IllegalArgument)
    }

    /// Array-valued attribute (`AXWindows`, `AXChildren`); non-element entries are skipped.
    pub fn attr_elements(&self, name: &'static str) -> AxResult<Vec<AxElement>> {
        let v = self.attr(name)?;
        let arr = v
            .downcast::<CFArray>()
            .map_err(|_| AXError::IllegalArgument)?;
        // SAFETY: every element of a CFArray is a CFType.
        let arr: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(arr) };
        Ok(arr.iter().filter_map(as_element).collect())
    }

    pub fn attr_point(&self, name: &'static str) -> AxResult<CGPoint> {
        as_point(&*self.attr(name)?).ok_or(AXError::IllegalArgument)
    }

    pub fn attr_size(&self, name: &'static str) -> AxResult<CGSize> {
        as_size(&*self.attr(name)?).ok_or(AXError::IllegalArgument)
    }

    fn set(&self, name: &'static str, value: &CFType) -> AxResult<()> {
        let name = CFString::from_static_str(name);
        // SAFETY: both are valid CF objects for the duration of the call.
        check(unsafe { self.0.set_attribute_value(&name, value) })
    }

    pub fn set_point(&self, name: &'static str, p: CGPoint) -> AxResult<()> {
        self.set(name, &*ax_value(AXValueType::CGPoint, &p)?)
    }

    pub fn set_size(&self, name: &'static str, s: CGSize) -> AxResult<()> {
        self.set(name, &*ax_value(AXValueType::CGSize, &s)?)
    }

    pub fn set_bool(&self, name: &'static str, on: bool) -> AxResult<()> {
        self.set(name, CFBoolean::new(on))
    }

    pub fn perform(&self, action: &'static str) -> AxResult<()> {
        let action = CFString::from_static_str(action);
        // SAFETY: `action` is a valid CFString.
        check(unsafe { self.0.perform_action(&action) })
    }

    /// The window's `CGWindowID`. No IPC: the id is cached in the element.
    pub fn window_id(&self) -> AxResult<u32> {
        let mut id = 0u32;
        // SAFETY: `id` is a valid out-pointer; the element is a live AXUIElementRef.
        check(unsafe { ax_ui_element_get_window(&self.0, &mut id) })?;
        Ok(id)
    }

    pub fn pid(&self) -> AxResult<i32> {
        let mut pid = 0;
        // SAFETY: `pid` is a valid out-pointer.
        check(unsafe { self.0.pid(NonNull::from(&mut pid)) })?;
        Ok(pid)
    }
}

fn ax_value<T>(ty: AXValueType, v: &T) -> AxResult<CFRetained<AXValue>> {
    // SAFETY: `v` points to a live value of the C type matching `ty`; AXValueCreate copies it.
    unsafe { AXValue::new(ty, NonNull::from(v).cast::<c_void>()) }.ok_or(AXError::Failure)
}

fn ax_value_get<T: Default>(v: &CFType, ty: AXValueType) -> Option<T> {
    let v = v.downcast_ref::<AXValue>()?;
    // SAFETY: reading the type tag has no preconditions.
    if unsafe { v.r#type() } != ty {
        return None;
    }
    let mut out = T::default();
    // SAFETY: `out` is a valid, writable value of the C type matching `ty` (checked above).
    unsafe { v.value(ty, NonNull::from(&mut out).cast::<c_void>()) }.then_some(out)
}

fn is_ax_error(v: &CFType) -> bool {
    // SAFETY: reading the type tag has no preconditions.
    v.downcast_ref::<AXValue>()
        .is_some_and(|v| unsafe { v.r#type() } == AXValueType::AXError)
}

pub fn as_string(v: &CFType) -> Option<String> {
    v.downcast_ref::<CFString>().map(|s| s.to_string())
}

pub fn as_bool(v: &CFType) -> Option<bool> {
    v.downcast_ref::<CFBoolean>().map(|b| b.as_bool())
}

pub fn as_point(v: &CFType) -> Option<CGPoint> {
    ax_value_get(v, AXValueType::CGPoint)
}

pub fn as_size(v: &CFType) -> Option<CGSize> {
    ax_value_get(v, AXValueType::CGSize)
}

pub fn as_element(v: CFRetained<CFType>) -> Option<AxElement> {
    v.downcast::<AXUIElement>().ok().map(AxElement)
}
