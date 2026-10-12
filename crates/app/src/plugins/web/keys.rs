//! Key translation from winit events to the web host's CEF key events.

use ghostrealm_web::protocol::mods;
use ghostrealm_web::{Key, KeyKind};
use winit::keyboard::{Key as WKey, KeyCode, NamedKey, PhysicalKey};

/// CEF modifier flags for the current modifier state.
pub fn modifier_flags(m: ghostrealm_terminal::Mods) -> u32 {
    let mut f = 0;
    if m.shift {
        f |= mods::SHIFT;
    }
    if m.ctrl {
        f |= mods::CONTROL;
    }
    if m.alt {
        f |= mods::ALT;
    }
    if m.super_ {
        f |= mods::COMMAND;
    }
    f
}

/// The key events (down + char, or up) a winit key event becomes.
pub fn translate(event: &winit::event::KeyEvent, unmodified: Option<char>, flags: u32) -> Vec<Key> {
    let native = native_code(event.physical_key);
    let vk = windows_code(&event.logical_key, event.physical_key);
    let special = special_char(&event.logical_key);
    let text: Option<char> = event.text.as_ref().and_then(|t| t.chars().next());
    let character = special.or(text).map(utf16).unwrap_or(0);
    let unmod = special.or(unmodified).or(text).map(utf16).unwrap_or(0);
    let key = |kind| Key {
        kind,
        windows_key_code: vk,
        native_key_code: native,
        character,
        unmodified_character: unmod,
        mods: flags,
    };
    if event.state == winit::event::ElementState::Released {
        return vec![key(KeyKind::Up)];
    }
    let mut out = vec![key(KeyKind::Down)];
    // Typed characters (and Enter/Tab/Backspace, which edit text) also need a
    // CHAR event; a Cmd/Ctrl chord does not insert text.
    let inserts = flags & (mods::COMMAND | mods::CONTROL) == 0;
    if inserts && (text.is_some_and(|c| !c.is_control()) || is_editing_key(&event.logical_key)) {
        out.push(key(KeyKind::Char));
    }
    out
}

fn utf16(c: char) -> u16 {
    let mut buf = [0u16; 2];
    c.encode_utf16(&mut buf)[0]
}

fn is_editing_key(k: &WKey) -> bool {
    matches!(
        k,
        WKey::Named(NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace)
    )
}

/// The character macOS reports for non-printing keys (`NSEvent.characters`),
/// which Chromium's Mac key decoding relies on.
fn special_char(k: &WKey) -> Option<char> {
    let c = match k {
        WKey::Named(n) => match n {
            NamedKey::Enter => '\r',
            NamedKey::Tab => '\t',
            NamedKey::Backspace => '\u{7f}',
            NamedKey::Escape => '\u{1b}',
            NamedKey::ArrowUp => '\u{f700}',
            NamedKey::ArrowDown => '\u{f701}',
            NamedKey::ArrowLeft => '\u{f702}',
            NamedKey::ArrowRight => '\u{f703}',
            NamedKey::Delete => '\u{f728}',
            NamedKey::Home => '\u{f729}',
            NamedKey::End => '\u{f72b}',
            NamedKey::PageUp => '\u{f72c}',
            NamedKey::PageDown => '\u{f72d}',
            NamedKey::F1 => '\u{f704}',
            NamedKey::F2 => '\u{f705}',
            NamedKey::F3 => '\u{f706}',
            NamedKey::F4 => '\u{f707}',
            NamedKey::F5 => '\u{f708}',
            NamedKey::F6 => '\u{f709}',
            NamedKey::F7 => '\u{f70a}',
            NamedKey::F8 => '\u{f70b}',
            NamedKey::F9 => '\u{f70c}',
            NamedKey::F10 => '\u{f70d}',
            NamedKey::F11 => '\u{f70e}',
            NamedKey::F12 => '\u{f70f}',
            _ => return None,
        },
        _ => return None,
    };
    if cfg!(target_os = "macos") || c.is_ascii() {
        Some(c)
    } else {
        None
    }
}

fn native_code(p: PhysicalKey) -> i32 {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        use winit::platform::scancode::PhysicalKeyExtScancode;
        p.to_scancode().map(|c| c as i32).unwrap_or(0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = p;
        0
    }
}

/// Windows virtual-key code (Chromium's `KeyboardCode`).
#[rustfmt::skip]
fn windows_code(k: &WKey, p: PhysicalKey) -> i32 {
    if let WKey::Named(n) = k {
        let vk = match n {
            NamedKey::Backspace => 0x08,
            NamedKey::Tab => 0x09,
            NamedKey::Enter => 0x0D,
            NamedKey::Shift => 0x10,
            NamedKey::Control => 0x11,
            NamedKey::Alt => 0x12,
            NamedKey::CapsLock => 0x14,
            NamedKey::Escape => 0x1B,
            NamedKey::Space => 0x20,
            NamedKey::PageUp => 0x21,
            NamedKey::PageDown => 0x22,
            NamedKey::End => 0x23,
            NamedKey::Home => 0x24,
            NamedKey::ArrowLeft => 0x25,
            NamedKey::ArrowUp => 0x26,
            NamedKey::ArrowRight => 0x27,
            NamedKey::ArrowDown => 0x28,
            NamedKey::Insert => 0x2D,
            NamedKey::Delete => 0x2E,
            NamedKey::Super | NamedKey::Meta => 0x5B,
            NamedKey::F1 => 0x70,
            NamedKey::F2 => 0x71,
            NamedKey::F3 => 0x72,
            NamedKey::F4 => 0x73,
            NamedKey::F5 => 0x74,
            NamedKey::F6 => 0x75,
            NamedKey::F7 => 0x76,
            NamedKey::F8 => 0x77,
            NamedKey::F9 => 0x78,
            NamedKey::F10 => 0x79,
            NamedKey::F11 => 0x7A,
            NamedKey::F12 => 0x7B,
            _ => 0,
        };
        if vk != 0 {
            return vk;
        }
    }
    // Letters/digits/punctuation by physical position (layout-independent,
    // as Chromium expects for VK codes).
    let PhysicalKey::Code(code) = p else { return 0 };
    use KeyCode::*;
    match code {
        KeyA => 0x41, KeyB => 0x42, KeyC => 0x43, KeyD => 0x44, KeyE => 0x45,
        KeyF => 0x46, KeyG => 0x47, KeyH => 0x48, KeyI => 0x49, KeyJ => 0x4A,
        KeyK => 0x4B, KeyL => 0x4C, KeyM => 0x4D, KeyN => 0x4E, KeyO => 0x4F,
        KeyP => 0x50, KeyQ => 0x51, KeyR => 0x52, KeyS => 0x53, KeyT => 0x54,
        KeyU => 0x55, KeyV => 0x56, KeyW => 0x57, KeyX => 0x58, KeyY => 0x59,
        KeyZ => 0x5A,
        Digit0 => 0x30, Digit1 => 0x31, Digit2 => 0x32, Digit3 => 0x33, Digit4 => 0x34,
        Digit5 => 0x35, Digit6 => 0x36, Digit7 => 0x37, Digit8 => 0x38, Digit9 => 0x39,
        Space => 0x20,
        Semicolon => 0xBA, Equal => 0xBB, Comma => 0xBC, Minus => 0xBD, Period => 0xBE,
        Slash => 0xBF, Backquote => 0xC0, BracketLeft => 0xDB, Backslash => 0xDC,
        BracketRight => 0xDD, Quote => 0xDE,
        _ => 0,
    }
}

/// The key's character with modifiers (Shift's symbol, Option's dead keys)
/// undone: Chromium's `unmodified_character`, and what Cmd chords match on.
pub fn base_char(event: &winit::event::KeyEvent) -> Option<char> {
    #[cfg(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    ))]
    let key = {
        use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
        event.key_without_modifiers()
    };
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    )))]
    let key = event.logical_key.clone();
    match key {
        WKey::Character(s) => s.chars().next(),
        WKey::Named(NamedKey::Space) => Some(' '),
        _ => None,
    }
}
