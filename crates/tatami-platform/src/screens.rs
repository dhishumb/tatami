//! Monitors in AX coordinates (global, top-left origin at the primary display).

use std::ptr::NonNull;

use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CFRetained, CFUUID, CGRect};
use objc2_core_graphics::{CGDirectDisplayID, CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{NSNumber, NSString};
use tatami_core::MonitorInfo;
use tatami_protocol::Rect;

#[link(name = "ColorSync", kind = "framework")]
unsafe extern "C" {
    /// Create rule: returns a +1 CFUUID, or null for an unknown display.
    #[link_name = "CGDisplayCreateUUIDFromDisplayID"]
    fn cg_display_create_uuid(display: CGDirectDisplayID) -> Option<NonNull<CFUUID>>;
}

fn rect(r: CGRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn display_uuid(id: CGDirectDisplayID) -> Option<String> {
    // SAFETY: any display id is accepted; null is handled.
    let uuid = unsafe { cg_display_create_uuid(id) }?;
    // SAFETY: Create rule, we own this reference.
    let uuid: CFRetained<CFUUID> = unsafe { CFRetained::from_raw(uuid) };
    CFUUID::new_string(None, Some(&uuid)).map(|s| s.to_string())
}

fn display_id(screen: &NSScreen) -> Option<CGDirectDisplayID> {
    let key = NSString::from_str("NSScreenNumber");
    let n = screen.deviceDescription().objectForKey(&key)?;
    n.downcast::<NSNumber>().ok().map(|n| n.unsignedIntValue())
}

pub fn screens(mtm: MainThreadMarker) -> Vec<MonitorInfo> {
    let main = CGMainDisplayID();
    // AppKit's origin is the bottom-left of the primary display; AX's is its top-left.
    let main_h = CGDisplayBounds(main).size.height;
    NSScreen::screens(mtm)
        .iter()
        .filter_map(|s| {
            let id = display_id(&s)?;
            let v = s.visibleFrame();
            let visible = Rect::new(
                v.origin.x,
                main_h - (v.origin.y + v.size.height),
                v.size.width,
                v.size.height,
            );
            Some(MonitorInfo {
                uuid: display_uuid(id).unwrap_or_else(|| format!("display-{id}")),
                name: s.localizedName().to_string(),
                frame: rect(CGDisplayBounds(id)),
                visible,
                is_main: id == main,
            })
        })
        .collect()
}
