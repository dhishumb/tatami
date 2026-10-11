//! The menu bar status item: a template icon, an optional short text after
//! it, and a menu whose items emit `PlatformEvent::Command`. Created lazily on
//! the first `set_status`/`set_menu` and never hidden afterwards, since it is
//! the user's way in.

use std::cell::RefCell;
use std::rc::Weak;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSCellImagePosition, NSControlStateValueOn, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObjectProtocol, NSString, ns_string};
use tatami_core::{MenuItem, PlatformEvent};

use crate::Shared;

/// SF Symbol shown in the menu bar.
const ICON: &str = "square.grid.2x2";
/// AppKit caps `indentationLevel` at 15.
const MAX_INDENT: u8 = 15;

struct TargetIvars {
    /// One entry per menu row; a row's `tag` is its index here.
    commands: RefCell<Vec<Vec<String>>>,
    shared: Weak<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no `Drop` impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TatamiMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        // SAFETY: the signature matches an action method `- (void)menuAction:(id)sender`.
        #[unsafe(method(menuAction:))]
        fn menu_action(&self, sender: &NSMenuItem) {
            let iv = self.ivars();
            // Clone out of the borrow: the sink may call `set_menu` re-entrantly.
            let cmd = usize::try_from(sender.tag())
                .ok()
                .and_then(|i| iv.commands.borrow().get(i).cloned())
                .filter(|c| !c.is_empty());
            if let (Some(cmd), Some(sh)) = (cmd, iv.shared.upgrade()) {
                sh.emit(PlatformEvent::Command(cmd));
            }
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, shared: Weak<Shared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars {
            commands: RefCell::default(),
            shared,
        });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

pub(crate) struct Status {
    shared: Weak<Shared>,
    item: Option<Retained<NSStatusItem>>,
    /// Menu items hold their target weakly; this keeps it alive.
    target: Option<Retained<MenuTarget>>,
    text: String,
    menu: Vec<MenuItem>,
}

impl Status {
    pub(crate) fn new(shared: Weak<Shared>) -> Self {
        Self {
            shared,
            item: None,
            target: None,
            text: String::new(),
            menu: Vec::new(),
        }
    }

    fn item(&mut self, mtm: MainThreadMarker) -> &NSStatusItem {
        self.item.get_or_insert_with(|| {
            let item =
                NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            if let Some(b) = item.button(mtm) {
                let icon = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(ICON),
                    Some(ns_string!("tatami")),
                );
                if let Some(icon) = &icon {
                    // Template images follow the menu bar's light/dark tint.
                    icon.setTemplate(true);
                }
                b.setImage(icon.as_deref());
                b.setImagePosition(NSCellImagePosition::ImageLeading);
                b.setToolTip(Some(ns_string!("tatami")));
            }
            item.setVisible(true);
            item
        })
    }

    /// Text after the icon; empty shows the icon alone.
    pub(crate) fn set(&mut self, mtm: MainThreadMarker, text: &str) {
        if self.item.is_some() && text == self.text {
            return;
        }
        text.clone_into(&mut self.text);
        if let Some(b) = self.item(mtm).button(mtm) {
            b.setTitle(&NSString::from_str(text));
        }
    }

    /// Replace the menu; a no-op when `items` equals the last set.
    pub(crate) fn set_menu(&mut self, mtm: MainThreadMarker, items: &[MenuItem]) {
        if self.item.is_some() && items == self.menu {
            return;
        }
        self.menu = items.to_vec();
        let target = self
            .target
            .get_or_insert_with(|| MenuTarget::new(mtm, self.shared.clone()))
            .clone();
        *target.ivars().commands.borrow_mut() = items.iter().map(|i| i.command.clone()).collect();
        let menu = (!items.is_empty()).then(|| build_menu(mtm, items, &target));
        self.item(mtm).setMenu(menu.as_deref());
    }
}

fn build_menu(mtm: MainThreadMarker, items: &[MenuItem], target: &MenuTarget) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    // Enabled state is exactly what we set: headers off, commands on.
    menu.setAutoenablesItems(false);
    for (i, row) in items.iter().enumerate() {
        if row.separator {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            continue;
        }
        let enabled = !row.command.is_empty();
        let action = enabled.then_some(sel!(menuAction:));
        // SAFETY: `menuAction:` is a valid selector implemented by `MenuTarget`.
        let it = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&row.title),
                action,
                ns_string!(""),
            )
        };
        it.setEnabled(enabled);
        it.setTag(i as isize);
        it.setIndentationLevel(row.indent.min(MAX_INDENT).into());
        if row.checked {
            it.setState(NSControlStateValueOn);
        }
        if enabled {
            // SAFETY: `target` responds to `menuAction:` and is kept alive by `Status`.
            unsafe { it.setTarget(Some(target)) };
        }
        menu.addItem(&it);
    }
    menu
}
