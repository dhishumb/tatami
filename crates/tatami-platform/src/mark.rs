//! The monitor outline: a border around one monitor while the overview's
//! highlight is on it, the way Displays settings marks the display you
//! pick. A transparent panel the size of the monitor whose content is an
//! `NSBox` border, above every window and under the sheet, never in the
//! way of the mouse.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBox, NSBoxType, NSColor, NSPanel,
    NSStatusWindowLevel, NSTitlePosition, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use tatami_core::MonitorInfo;

const WIDTH: f64 = 6.0;
const RADIUS: f64 = 12.0;
/// The sheet's accent blue.
const COLOR: (f64, f64, f64) = (0.45, 0.72, 1.0);

pub(crate) struct Mark {
    panel: Option<Retained<NSPanel>>,
    /// Uuid of the monitor outlined now.
    shown: Option<String>,
}

impl Mark {
    pub(crate) fn new() -> Self {
        Self {
            panel: None,
            shown: None,
        }
    }

    pub(crate) fn show(&mut self, mtm: MainThreadMarker, monitor: Option<&MonitorInfo>) {
        let Some(m) = monitor else {
            if self.shown.take().is_some()
                && let Some(p) = &self.panel
            {
                p.orderOut(None);
            }
            return;
        };
        if self.shown.as_deref() == Some(m.uuid.as_str()) {
            return;
        }
        let panel = self.panel.get_or_insert_with(|| make_panel(mtm));
        let f = m.frame;
        // AX frames are top-left based; AppKit's origin is the primary display's bottom-left.
        let main_h = CGDisplayBounds(CGMainDisplayID()).size.height;
        let rect = CGRect::new(
            CGPoint::new(f.x, main_h - (f.y + f.h)),
            CGSize::new(f.w, f.h),
        );
        panel.setFrame_display(rect, true);
        panel.orderFrontRegardless();
        self.shown = Some(m.uuid.clone());
    }
}

fn make_panel(mtm: MainThreadMarker) -> Retained<NSPanel> {
    let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(10.0, 10.0));
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    // Just under the sheet, so where the two overlap the sheet wins.
    panel.setLevel(NSStatusWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel.setIgnoresMouseEvents(true);
    panel.setMovable(false);
    panel.setHasShadow(false);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    // Our app is never active; the default would hide the panel at once.
    panel.setHidesOnDeactivate(false);
    // SAFETY: we hold the only strong reference; AppKit must not release it on close.
    unsafe { panel.setReleasedWhenClosed(false) };

    let border = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    border.setBoxType(NSBoxType::Custom);
    border.setTitlePosition(NSTitlePosition::NoTitle);
    border.setBorderWidth(WIDTH);
    border.setCornerRadius(RADIUS);
    border.setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        COLOR.0, COLOR.1, COLOR.2, 0.95,
    ));
    border.setFillColor(&NSColor::clearColor());
    border.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    panel.setContentView(Some(&border));
    panel
}
