//! AppKit ends the app without a close request when asked to terminate: the
//! app menu's Quit, the Dock's Quit, logout and shutdown, and ⌘Q while no
//! window is key (the key monitor only sees the window's own keys). winit's
//! application delegate has no `applicationShouldTerminate:`, so termination
//! goes straight to `applicationWillTerminate:` and the process exits with
//! unsaved changes dropped. Adding that method lets the app cancel the
//! termination when there is something to ask about, and quit its own way:
//! asking first, exiting once agreed. With nothing to ask, termination goes
//! ahead as before (so a logout isn't held up).

use std::cell::RefCell;

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{ffi, sel, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};

thread_local! {
    /// Asked whether to terminate now. AppKit asks on the main thread.
    static ON_TERMINATE: RefCell<Option<Box<dyn Fn() -> bool>>> = const { RefCell::new(None) };
}

/// Ask `on_request` before every termination, for the life of the app: it
/// returns whether to terminate now; otherwise the termination is cancelled
/// (it is the app's to quit later). Call once, on the main thread, once the
/// event loop runs (winit sets the application delegate then).
pub fn intercept_terminate(on_request: impl Fn() -> bool + 'static) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(delegate) = NSApplication::sharedApplication(mtm).delegate() else {
        return;
    };
    let object: &AnyObject = delegate.as_ref();
    install(object.class(), on_request);
}

/// Give `class` an `applicationShouldTerminate:` that asks `on_request`.
/// Returns false if the class already has one of its own.
fn install(class: &AnyClass, on_request: impl Fn() -> bool + 'static) -> bool {
    ON_TERMINATE.with(|f| *f.borrow_mut() = Some(Box::new(on_request)));
    type ShouldTerminate = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> NSApplicationTerminateReply;
    let imp: ShouldTerminate = should_terminate;
    // SAFETY: the function matches the method's signature, which `types`
    // spells out (`NSUInteger (id self, SEL _cmd, id sender)`); the runtime
    // calls an `Imp` with the method's own arguments.
    unsafe {
        ffi::class_addMethod(
            (class as *const AnyClass).cast_mut(),
            sel!(applicationShouldTerminate:),
            std::mem::transmute::<ShouldTerminate, Imp>(imp),
            c"Q@:@".as_ptr(),
        )
    }
    .as_bool()
}

extern "C-unwind" fn should_terminate(
    _this: *mut AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) -> NSApplicationTerminateReply {
    let now = ON_TERMINATE.with(|f| f.borrow().as_ref().is_none_or(|f| f()));
    if now {
        NSApplicationTerminateReply::TerminateNow
    } else {
        NSApplicationTerminateReply::TerminateCancel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    use objc2::msg_send;
    use objc2::runtime::{ClassBuilder, NSObject};
    use objc2::ClassType;

    #[test]
    fn a_termination_request_asks_the_app_and_cancels_unless_told_to_go_ahead() {
        let class = ClassBuilder::new(c"GhostrealmTestAppDelegate", NSObject::class())
            .expect("a fresh class name")
            .register();
        let asked = Rc::new(Cell::new(0));
        let go_ahead = Rc::new(Cell::new(false));
        let (seen, now) = (asked.clone(), go_ahead.clone());
        assert!(install(class, move || {
            seen.set(seen.get() + 1);
            now.get()
        }));

        let delegate: *mut AnyObject = unsafe { msg_send![class, new] };
        let ask = || -> NSApplicationTerminateReply {
            unsafe { msg_send![delegate, applicationShouldTerminate: std::ptr::null_mut::<AnyObject>()] }
        };
        assert_eq!(ask(), NSApplicationTerminateReply::TerminateCancel);
        assert_eq!(asked.get(), 1);
        go_ahead.set(true);
        assert_eq!(ask(), NSApplicationTerminateReply::TerminateNow);
        assert_eq!(asked.get(), 2);
        let _: () = unsafe { msg_send![delegate, release] };
    }
}
