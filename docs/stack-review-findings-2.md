# Stack review findings (2)

Open bugs found reviewing `1e1ff67..8a77896`. "Confirmed" means a throwaway
test reproduced it; the rest come from reading the code. Items marked
"Decision needed" wait on a product call.

## Low

7. **Terminal readline translation applies inside full-screen apps.** Word/line
   motions become readline keys ([`plugins/terminal.rs:590`](../crates/app/src/plugins/terminal.rs),
   from `903b54d`) in the alternate screen too, and on Linux/Windows they replace
   the Ctrl+arrow/Home/End CSI sequences. Decision needed: translate only
   outside the alternate screen / application-cursor mode, or always (as iTerm's
   "natural text editing" does)?
14. **Missing `[keybindings]` binds nothing, silently** (`019dc17`). Files
    written before then carry the comment "If the whole section is empty, the
    built-in defaults apply", which is now false. Decision needed: a missing
    section means the defaults, or stays strict with a warning finding?
15. **Load-time findings go only to stderr** (`load_or_create`,
    [`config.rs:470`](../crates/core/src/config.rs), `019dc17`). New clamps
    (`sidebar.width` min 40, `font_size` max 96, `double_tap_window_ms` max 2000)
    change existing files' behaviour silently when launched from Finder.
    Decision needed: surface them how (inbox, toast, settings status)?

## Web browser and file-browser preview

Found reviewing `8a77896..d34d7fa` (the web browser `0c67045`/`35e68b9`/`04794e1`
and the file-browser preview `3f58197`/`b6e3944`/`d34d7fa`). Line numbers are
against `6611d14`.

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
    `ghostrealm agent` running `web.new`) can't start its host; the pane says the
    profile is in use. Decision needed: share cookies and logins between the GUI
    and the agent, or a profile per process?
24. **A preview can block on a stuck mount.** `load`'s `metadata` call
    ([`plugins/file_browser/preview.rs:69`](../crates/app/src/plugins/file_browser/preview.rs))
    blocks the loader on a dead network mount. Decision needed: a thread or
    timeout per request?
31. **Popups load in place and lose `window.opener`** (`on_before_popup`,
    [`browser.rs:626`](../crates/web-host/src/browser.rs)), which breaks OAuth and
    payment popup flows. Decision needed: open them as a new tab in the pane?
32. **Image mips average sRGB-encoded values** (`halve`,
    [`plugin/paint.rs:266`](../crates/app/src/plugin/paint.rs)), so a shrunk
    image with fine detail comes out slightly dark. Fix: average in linear light.
33. **Autoplay can start audio in a hidden tab.** `tick`
    ([`preview.rs:338`](../crates/app/src/plugins/file_browser/preview.rs)) runs
    for hidden views, and only `paint` pauses. Decision needed: should hidden
    audio keep playing? If not, clear `autoplay_at` and pause in `View::shown`.
34. **The preview follows hover.** Moving the mouse to the preview's controls
    crosses rows, switches the subject and pauses the track; in Browse mode,
    clicking an audio row opens the editor. Decision needed: follow the
    selection (as Finder does), pin to the chosen row, or hold the subject while
    the pointer travels to the preview?
