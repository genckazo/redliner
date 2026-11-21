//! Redline's menu-bar item on macOS: an `NSStatusItem` with a small menu. Rune has no macOS
//! status item yet (its `Tray` covers Linux and Windows), so this talks to AppKit directly.
//! Everything here must run on the main thread; off it, the calls do nothing.

#![allow(unsafe_code)]

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSMenu, NSMenuItem, NSStatusBar, NSStatusItem};
use objc2_foundation::{NSObject, NSString};

/// What a menu entry does when picked.
pub type Action = Box<dyn Fn()>;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RedlineMenuTarget"]
    #[ivars = Action]
    struct Target;

    impl Target {
        #[unsafe(method(redlineMenuAction:))]
        fn perform(&self, _sender: &NSMenuItem) {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.ivars())())).is_err() {
                eprintln!("redline: a menu action panicked");
            }
        }
    }
);

thread_local! {
    /// The status item and its menu targets, kept for as long as the app runs.
    static INSTALLED: RefCell<Option<(Retained<NSStatusItem>, Vec<Retained<Target>>)>> = const { RefCell::new(None) };
}

/// Add the menu-bar item once. `entries` are (label, action); `None` is a separator.
pub fn install(title: &str, entries: Vec<Option<(&str, Action)>>) {
    let Some(mtm) = MainThreadMarker::new() else { eprintln!("redline: menu bar skipped, not on the main thread"); return };
    if INSTALLED.with(|slot| slot.borrow().is_some()) { return; }
    let menu = NSMenu::new(mtm);
    let mut targets = Vec::new();
    // SAFETY: on the main thread (mtm). Each item's target lives in INSTALLED for as long as the
    // menu holds the item, and the selector matches `Target::perform`'s signature.
    let item = unsafe {
        for entry in entries {
            let Some((label, action)) = entry else {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            };
            let row = NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(label), Some(sel!(redlineMenuAction:)), &NSString::from_str(""));
            let target: Retained<Target> = msg_send![super(mtm.alloc().set_ivars(action)), init];
            row.setTarget(Some(&target));
            targets.push(target);
            menu.addItem(&row);
        }
        // -1 is NSVariableStatusItemLength: as wide as the title.
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
        item.setMenu(Some(&menu));
        item
    };
    INSTALLED.with(|slot| *slot.borrow_mut() = Some((item, targets)));
    set_title(title);
    eprintln!("redline: menu bar item installed");
}

/// Change the text shown in the menu bar.
pub fn set_title(title: &str) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    INSTALLED.with(|slot| {
        if let Some((item, _)) = slot.borrow().as_ref() {
            if let Some(button) = item.button(mtm) { button.setTitle(&NSString::from_str(title)); }
        }
    });
}
