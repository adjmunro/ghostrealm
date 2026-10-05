//! Chromium's macOS message pump requires `NSApp` to implement
//! `CrAppControlProtocol` (it tracks whether an event is being dispatched). The
//! host has no windows, so the subclass only adds that bookkeeping.

use std::cell::Cell;

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObjectProtocol};
use objc2::{define_class, extern_methods, msg_send, ClassType, DefinedClass, MainThreadMarker};
use objc2_app_kit::{NSApp, NSApplication, NSApplicationActivationPolicy, NSEvent};

#[derive(Default)]
pub struct HostApplicationIvars {
    handling_send_event: Cell<Bool>,
}

define_class!(
    #[unsafe(super(NSApplication))]
    #[ivars = HostApplicationIvars]
    pub struct HostApplication;

    impl HostApplication {
        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self, event: &NSEvent) {
            let was = self.ivars().handling_send_event.get();
            self.ivars().handling_send_event.set(Bool::YES);
            let _: () = msg_send![super(self), sendEvent: event];
            self.ivars().handling_send_event.set(was);
        }
    }

    unsafe impl NSObjectProtocol for HostApplication {}

    unsafe impl CrAppControlProtocol for HostApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn _set_handling_send_event(&self, handling: Bool) {
            self.ivars().handling_send_event.set(handling);
        }
    }

    unsafe impl CrAppProtocol for HostApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn _is_handling_send_event(&self) -> Bool {
            self.ivars().handling_send_event.get()
        }
    }

    unsafe impl CefAppProtocol for HostApplication {}
);

impl HostApplication {
    extern_methods! {
        #[unsafe(method(sharedApplication))]
        fn shared_application() -> Retained<Self>;
    }
}

/// Make `NSApp` a [`HostApplication`] with no Dock icon or menu bar. Must run on
/// the main thread before anything else touches `NSApp`.
pub fn setup_application() {
    let _ = HostApplication::shared_application();
    let mtm = MainThreadMarker::new().expect("web host must start on the main thread");
    let app = NSApp(mtm);
    assert!(app.isKindOfClass(HostApplication::class()));
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
}
