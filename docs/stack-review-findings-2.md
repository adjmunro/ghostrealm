# Stack review findings (2)

Open bugs found reviewing `1e1ff67..8a77896`. "Confirmed" means a throwaway
test reproduced it; the rest come from reading the code. Items marked
"Decision needed" wait on a product call.

## Low

7. **Terminal readline translation applies inside full-screen apps.** Word/line
   motions become readline keys ([`plugins/terminal.rs:651`](../crates/app/src/plugins/terminal.rs),
   from `903b54d`) in the alternate screen too, and on Linux/Windows they replace
   the Ctrl+arrow/Home/End CSI sequences. Decision needed: translate only
   outside the alternate screen / application-cursor mode, or always (as iTerm's
   "natural text editing" does)?
   **Decision pending:** the owner wants the text-editing keys to do
   the same thing inside Vim, not Vim's own meaning of the readline bytes. The
   proposal is a translation layer: in the alternate screen, send standard
   xterm key sequences (Option+Left -> `CSI 1;3D`, Cmd+Left -> Home) instead of
   readline bytes (`ESC b`, `^A`), so Vim can map them in insert mode (normal
   mode has no "word left" for those keys without mappings). Alternative: turn
   the translation off in full-screen apps. Tracked in TODO.md.
14. **Missing `[keybindings]` binds nothing, silently** (`019dc17`). Files
    written before then carry the comment "If the whole section is empty, the
    built-in defaults apply", which is now false. **Decided:** a missing or empty
    `[keybindings]` binds nothing, deliberately, with no fallback to built-in
    defaults: every binding is explicit in the user's file. See DECISION.md
    (Config). Recommended bindings are tracked in TODO.md.
15. **Load-time findings go only to stderr** (`load_or_create`,
    [`config.rs:470`](../crates/core/src/config.rs), `019dc17`). New clamps
    (`sidebar.width` min 40, `font_size` max 96, `double_tap_window_ms` max 2000)
    change existing files' behaviour silently when launched from Finder.
    Decision needed: surface them how (inbox, toast, settings status)? Folded
    into the "User-visible error surface" design in TODO.md.

## Web browser and file-browser preview

Found reviewing `8a77896..d34d7fa` (the web browser `0c67045`/`35e68b9`/`04794e1`
and the file-browser preview `3f58197`/`b6e3944`/`d34d7fa`). Line numbers are
against `f59386c`.

Right and middle clicks were lost when the browser was rebased (`465b076` →
`04794e1`): `MouseEvent` carries no button, so only left clicks reach a page.

### High

#### 21. Chromium runs without its sandbox

- Where: `no_sandbox: 1` ([`web-host/src/main.rs:113`](../crates/web-host/src/main.rs)),
  for every process. Recorded in DECISION.md as an open risk.
- Effect: a renderer exploit runs with the user's full rights (files, SSH keys,
  and the app's shells).
- Decision needed: ship before sandboxing, or enable it (`cef_sandbox` in the
  macOS helpers, the setuid/namespace sandbox on Linux)?
- **Decided:** enable it, with no runtime flag to turn it off. See DECISION.md
  (Web surface / security); work tracked in TODO.md.

### Low

18. **`web.eval` runs in the page's JavaScript world**, so the page can shape
    its result (by replacing `console.log`, `eval` or `JSON.stringify`); the
    command's description says so. Decision needed: evaluate in an isolated
    world instead?
    **Decided:** yes, an isolated world (HIGH priority within the web work; an
    agent-driven browser is a prompt-injection and misuse risk). Tracked in
    TODO.md.
19. **Every ghostrealm process shares one web profile**
    (`~/Library/Caches/ghostrealm/web`), so a second process (another GUI, or
    `ghostrealm agent` running `web.new`) can't start its host; the pane (and the
    agent's `web.new`) says the profile is in use. Decision needed: share cookies and logins between the GUI
    and the agent, or a profile per process? Leaning towards one shared
    ghostrealm profile (GUI and agent), isolated from other browsers on the
    machine; not final.
    **Decided:** the profile moves to Application Support. Open design: the
    profile model (picker, human-only vs agent, scope) — see TODO.md
    ("Web — profiles").
24. **A preview can block on a stuck mount.** `load`'s `metadata` call
    ([`plugins/file_browser/preview.rs:73`](../crates/app/src/plugins/file_browser/preview.rs))
    blocks the loader on a dead network mount. Decision needed: a thread or
    timeout per request?
31. **Popups load in place and lose `window.opener`** (`on_before_popup`,
    [`browser.rs:626`](../crates/web-host/src/browser.rs)), which breaks OAuth and
    payment popup flows. Decision needed: open them as a new tab in the pane?
    **Decided:** a human-triggered popup opens as a floating pop-up window above
    the app with the original tab kept alive (fallback: a new tab); an
    agent-triggered popup opens hidden in the background. See DECISION.md
    (UX principle / agent-operated surfaces); work tracked in TODO.md.
33. **Autoplay can start audio in a hidden tab.** `tick`
    ([`preview.rs:343`](../crates/app/src/plugins/file_browser/preview.rs)) runs
    for hidden views, and only `paint` pauses. **Decided:** the file
    browser does not play audio while hidden; clear `autoplay_at` and pause in
    `View::shown`. Tracked in TODO.md.
34. **The preview follows hover.** Moving the mouse to the preview's controls
    crosses rows, switches the subject and pauses the track; in Browse mode,
    clicking an audio row opens the editor. Decision needed: follow the
    selection (as Finder does), pin to the chosen row, or hold the subject while
    the pointer travels to the preview? The user will try the behaviour before
    deciding; tracked in TODO.md.

## Terminal input

Found reviewing `4981b3a..d5e17a2` (bracketed paste and mouse reporting
`df032a4`/`0cae15a`).

### Low

35. **Mouse reports go to the wrong row while the viewport is scrolled back.**
    `report_mouse` ([`plugins/terminal.rs`](../crates/app/src/plugins/terminal.rs))
    reports the pointer's cell in the scrolled viewport, but the program's screen
    is the live one, so a click lands rows away from the pointer (Ghostty upstream
    behaves the same). Decided: never snap to the bottom (the owner may be
    copying); while the viewport is scrolled back the mouse acts locally and is
    not reported. Fix tracked in TODO.md.
36. **Unbracketed paste sends control bytes raw, and LF rather than CR**
    (`GhosttyTerminal::encode_paste`,
    [`terminal-ghostty/src/lib.rs`](../crates/terminal-ghostty/src/lib.rs)). A
    paste into a program that didn't enable mode 2004 can carry ESC or other
    control bytes as keystrokes, and a newline arrives as LF. xterm and Ghostty
    strip the controls and convert LF to CR (`paste::encode(.., false, ..)`). The
    backend test `paste_is_unwrapped_when_the_program_did_not_ask_for_brackets`
    locks in the raw behaviour. Decision needed: sanitise unbracketed pastes the
    same way?

## Follow-up fixes (f59386c..e534aa0)

Reviewed after these commits had reached `develop`. Found reviewing the
follow-up fixes (`3f99752`, `8493dac`, `0cc6b0c`, `01572a4`, `e9617b8`,
`55cc01c` and the commits around them). Each fix's test was revert-checked and
fails without the fix. Line numbers are against `950b88a`.

### Low–Medium

### Low

#### 42. Hover nits

- Where: `restrict` call site
  ([`window.rs:3281`](../crates/app/src/window.rs)), from `e9617b8`.
- `restrict` ignores `mouse_down`, so a modal opened from the keyboard
  mid-press sends `Leave` mid-press. That contradicts the
  [`event.rs`](../crates/app/src/plugin/event.rs) doc ("never during a press"),
  and the web view sends CEF a leave without button flags. Fix: skip `restrict`
  while `mouse_down`, or apply it on release.
- Related, pre-existing: switching tab or workspace by keyboard leaves hover on
  a view that is now hidden. Restrict hover to views painted that frame.

### Informational

#### 43. Image alpha blends in linear light

- Where: [`images.rs`](../crates/app/src/images.rs) and
  [`plugin/paint.rs`](../crates/app/src/plugin/paint.rs), from `55cc01c`.
- Effect: premultiplying and mip-building in linear light is physically correct
  and matches quads and text, but anti-aliased PNG edges look different from
  browsers and image viewers, which blend in encoded space.
- Decision needed: keep linear (current), or blend images in encoded space to
  match browsers?
