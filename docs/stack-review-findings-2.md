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
   **Decided:** a TOML setting, boolean, default true (translate in full-screen
   programs too). Tracked in TODO.md.
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

### Low

18. **`web.eval` runs in the page's JavaScript world**, so the page can shape
    its result (by replacing `console.log`, `eval` or `JSON.stringify`); the
    command's description says so. Decision needed: evaluate in an isolated
    world instead?
19. **Every ghostrealm process shares one web profile**
    (`~/Library/Caches/ghostrealm/web`), so a second process (another GUI, or
    `ghostrealm agent` running `web.new`) can't start its host; the pane (and the
    agent's `web.new`) says the profile is in use. Decision needed: share cookies and logins between the GUI
    and the agent, or a profile per process? Leaning towards one shared
    ghostrealm profile (GUI and agent), isolated from other browsers on the
    machine; not final.
24. **A preview can block on a stuck mount.** `load`'s `metadata` call
    ([`plugins/file_browser/preview.rs:73`](../crates/app/src/plugins/file_browser/preview.rs))
    blocks the loader on a dead network mount. Decision needed: a thread or
    timeout per request?
31. **Popups load in place and lose `window.opener`** (`on_before_popup`,
    [`browser.rs:626`](../crates/web-host/src/browser.rs)), which breaks OAuth and
    payment popup flows. Decision needed: open them as a new tab in the pane?
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
    behaves the same). Decision needed: snap to the bottom on a reporting
    press/wheel, or don't report while `viewport_active()` is false?
36. **Unbracketed paste sends control bytes raw, and LF rather than CR**
    (`GhosttyTerminal::encode_paste`,
    [`terminal-ghostty/src/lib.rs`](../crates/terminal-ghostty/src/lib.rs)). A
    paste into a program that didn't enable mode 2004 can carry ESC or other
    control bytes as keystrokes, and a newline arrives as LF. xterm and Ghostty
    strip the controls and convert LF to CR (`paste::encode(.., false, ..)`). The
    backend test `paste_is_unwrapped_when_the_program_did_not_ask_for_brackets`
    locks in the raw behaviour. Decision needed: sanitise unbracketed pastes the
    same way?
