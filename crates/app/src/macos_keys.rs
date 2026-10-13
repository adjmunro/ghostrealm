//! macOS reserves some chords for menu items: AppKit consumes their key-down
//! as a menu key equivalent before the window sees it, so winit gets at most
//! the key-up (⇧⌘/, "Show Help menu") or nothing (⌘Q, which ends the process
//! without a close request). A local event monitor runs before that dispatch;
//! it hands the key-down straight to winit's content view, so the chord reaches
//! the app like any other.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::MainThreadOnly;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSView};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// Route the system-reserved chords to `window`'s view for the life of the
/// app. Call once, on the main thread, after the window exists.
pub fn route_reserved_chords(window: &Window) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return;
    };
    // SAFETY: winit's AppKit handle points at the window's live content view,
    // which outlives the app's single window; retaining it keeps it valid.
    let Some(view) = (unsafe { Retained::retain(h.ns_view.as_ptr().cast::<NSView>()) }) else {
        return;
    };
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes a valid event for the duration of the call.
        let ev = unsafe { event.as_ref() };
        if is_reserved_chord(ev) && ev.window(view.mtm()) == view.window() {
            view.keyDown(ev);
            return std::ptr::null_mut();
        }
        event.as_ptr()
    });
    // SAFETY: the handler returns the event it was given or null, as required.
    let monitor =
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler) };
    // The monitor lives as long as the app; it is never removed.
    std::mem::forget(monitor);
}

/// ⌘? (Shift+/ on layouts where `?` is shifted `/`) or ⌘Q, with no Ctrl/Option.
fn is_reserved_chord(ev: &NSEvent) -> bool {
    if ev.r#type() != NSEventType::KeyDown {
        return false;
    }
    let flags = ev.modifierFlags() & NSEventModifierFlags::DeviceIndependentFlagsMask;
    let flags = flags & !NSEventModifierFlags::CapsLock;
    let Some(key) = ev.charactersIgnoringModifiers().map(|s| s.to_string()) else {
        return false;
    };
    let help = NSEventModifierFlags::Command | NSEventModifierFlags::Shift;
    (flags == help && key == "?")
        || (flags == NSEventModifierFlags::Command && key.eq_ignore_ascii_case("q"))
}
