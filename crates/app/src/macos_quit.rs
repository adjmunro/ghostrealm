//! AppKit ends the app without a close request when asked to terminate: the
//! app menu's Quit, the Dock's Quit, logout and shutdown, and ⌘Q while no
//! window is key (the key monitor only sees the window's own keys). winit's
//! application delegate has no `applicationShouldTerminate:`, so termination
//! goes straight to `applicationWillTerminate:` and the process exits with
//! unsaved changes dropped. Adding that method lets the app hold the
//! termination while it checks for something to ask about, then answer with
//! [`reply_to_terminate`]: yes to go ahead, no to stay and ask first. With
//! nothing to ask, termination goes ahead at once (so a logout isn't held up).

use std::cell::RefCell;

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{ffi, sel, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};

thread_local! {
    /// Asked whether to terminate now. AppKit asks on the main thread.
    static ON_TERMINATE: RefCell<Option<Box<dyn Fn() -> Terminate>>> = const { RefCell::new(None) };
}

/// The app's answer to a termination request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminate {
    /// Terminate straight away.
    Now,
    /// Hold the termination until the app calls [`reply_to_terminate`].
    Later,
}

/// Ask `on_request` before every termination, for the life of the app. After
/// [`Terminate::Later`] the app must call [`reply_to_terminate`]. Call once,
/// on the main thread, once the event loop runs (winit sets the application
/// delegate then).
pub fn intercept_terminate(on_request: impl Fn() -> Terminate + 'static) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(delegate) = NSApplication::sharedApplication(mtm).delegate() else {
        return;
    };
    let object: &AnyObject = delegate.as_ref();
    install(object.class(), on_request);
}

/// Answer a termination held with [`Terminate::Later`]: `go_ahead` terminates,
/// otherwise the app stays. Does nothing off the main thread.
pub fn reply_to_terminate(go_ahead: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    NSApplication::sharedApplication(mtm).replyToApplicationShouldTerminate(go_ahead);
}

/// [`reply_to_terminate`], sent once the current run-loop callback has
/// returned. A yes can make AppKit call `applicationWillTerminate:` before the
/// reply call returns, which winit panics on if its own event handler is still
/// running ("another event is currently being handled").
pub fn reply_to_terminate_later(go_ahead: bool) {
    schedule_reply(go_ahead, |run| dispatch2::DispatchQueue::main().exec_async(run), reply_to_terminate);
}

/// Hand `enqueue` a job that calls `reply(go_ahead)`; nothing is replied
/// until the job runs.
fn schedule_reply(
    go_ahead: bool,
    enqueue: impl FnOnce(Box<dyn FnOnce() + Send>),
    reply: fn(bool),
) {
    enqueue(Box::new(move || reply(go_ahead)));
}

/// Give `class` an `applicationShouldTerminate:` that asks `on_request`.
/// Returns false if the class already has one of its own.
fn install(class: &AnyClass, on_request: impl Fn() -> Terminate + 'static) -> bool {
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
    match ON_TERMINATE.with(|f| f.borrow().as_ref().map_or(Terminate::Now, |f| f())) {
        Terminate::Now => NSApplicationTerminateReply::TerminateNow,
        Terminate::Later => NSApplicationTerminateReply::TerminateLater,
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
    fn a_scheduled_reply_is_sent_only_when_the_queued_job_runs() {
        use std::sync::atomic::{AtomicI8, Ordering};
        static SENT: AtomicI8 = AtomicI8::new(-1);
        fn record(go_ahead: bool) {
            SENT.store(go_ahead as i8, Ordering::SeqCst);
        }
        let mut queued: Option<Box<dyn FnOnce() + Send>> = None;
        schedule_reply(true, |job| queued = Some(job), record);
        assert_eq!(SENT.load(Ordering::SeqCst), -1, "nothing is sent while the caller is still running");
        queued.take().expect("a job was queued")();
        assert_eq!(SENT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_termination_request_asks_the_app_and_is_held_unless_told_to_go_ahead() {
        let class = ClassBuilder::new(c"GhostrealmTestAppDelegate", NSObject::class())
            .expect("a fresh class name")
            .register();
        let asked = Rc::new(Cell::new(0));
        let go_ahead = Rc::new(Cell::new(false));
        let (seen, now) = (asked.clone(), go_ahead.clone());
        assert!(install(class, move || {
            seen.set(seen.get() + 1);
            if now.get() {
                Terminate::Now
            } else {
                Terminate::Later
            }
        }));

        let delegate: *mut AnyObject = unsafe { msg_send![class, new] };
        let ask = || -> NSApplicationTerminateReply {
            unsafe { msg_send![delegate, applicationShouldTerminate: std::ptr::null_mut::<AnyObject>()] }
        };
        assert_eq!(ask(), NSApplicationTerminateReply::TerminateLater);
        assert_eq!(asked.get(), 1);
        go_ahead.set(true);
        assert_eq!(ask(), NSApplicationTerminateReply::TerminateNow);
        assert_eq!(asked.get(), 2);
        let _: () = unsafe { msg_send![delegate, release] };
    }
}
