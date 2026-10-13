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
against `d23ace3`. Item 20's cause is partly inferred.

Lost when the browser was rebased (`465b076` → `04794e1`): items 29 (Cmd+L in
the URL bar), 30, 36 and 37. Right and middle clicks were lost too: `MouseEvent`
carries no button, so only left clicks reach a page.

### High

#### 20. The host can outlive the app and hold the profile lock (confirmed)

- Where: [`web-host/src/main.rs:86`](../crates/web-host/src/main.rs) (settings,
  message loop, `shutdown`), `shut_down` ([`browser.rs:293`](../crates/web-host/src/browser.rs)).
- Found: an orphaned host (parent pid 1) alive for 4 hours, holding the profile.
- Cause (inferred): a second launch against the same profile reaches
  `on_already_running_app_relaunch`, which is unhandled, so CEF opens its own
  Chrome-style browser that isn't in `BROWSERS`; `shut_down` never sees it close
  and the message loop never quits.
- Fix: handle `on_already_running_app_relaunch` (return 1); exit from a
  watchdog once stdin hits EOF and shutdown hasn't finished within a timeout.

#### 21. Chromium runs without its sandbox

- Where: `no_sandbox: 1` ([`web-host/src/main.rs:87`](../crates/web-host/src/main.rs)),
  for every process. Not recorded in DECISION.md.
- Effect: a renderer exploit runs with the user's full rights (files, SSH keys,
  and the app's shells).
- Decision needed: ship before sandboxing, or enable it (`cef_sandbox` in the
  macOS helpers, the setuid/namespace sandbox on Linux)? At minimum record the
  trade-off.

### Medium

#### 22. A zero-width or zero-height image kills the preview loader (confirmed, fuzzed)

- Where: `Image::from_rgba` ([`plugin/paint.rs:192`](../crates/app/src/plugin/paint.rs))
  bumps the size to 1 and `halve` (`:258`) indexes the empty pixels; reached from
  `decode_image` ([`plugins/file_browser/preview.rs:120`](../crates/app/src/plugins/file_browser/preview.rs)).
- Repro: a 30-byte 17×0 GIF.
- Effect: the loader thread panics; previews stop for that pane, and `tick` keeps
  polling every 15 ms forever (~66 redraws/s).
- Fix: reject `w == 0 || h == 0` as `Content::Error`; catch a panic in `load`;
  stop polling once the loader is gone.

#### 23. Writing to a host that doesn't read blocks the UI thread (confirmed)

- Where: `WebHost::send` ([`web/src/client.rs:171`](../crates/web/src/client.rs)),
  a blocking `write_all` under a mutex, called from paint, mouse and key handlers.
- Effect: once the 64 KiB pipe fills (a host stuck in CEF), the whole app
  freezes, shells included.
- Fix: a writer thread behind a bounded queue; drop mouse moves when it is full.

#### 24. A FIFO or device node hangs the preview loader (confirmed)

- Where: `load` → `read_text` ([`preview.rs:109`](../crates/app/src/plugins/file_browser/preview.rs),
  `:139`); `audio::duration` likewise.
- Effect: the loader blocks in `open`/`read` forever (and consumes the pipe's
  data); the thread leaks.
- Fix: read only when `meta.is_file()`; show Pipe/Device/Socket with no content.
  A `metadata` call stuck on a dead network mount needs a per-request thread or
  timeout: a design choice, not covered here.

#### 25. The audio device stays open after playback ends (confirmed)

- Where: `run` / `start` ([`plugins/file_browser/audio.rs:96`](../crates/app/src/plugins/file_browser/audio.rs),
  `:164`) keep the output device for the thread's life.
- Effect: macOS holds a `PreventUserIdleSystemSleep` assertion for as long as
  the view lives.
- Fix: drop the device once playback finishes or pauses (after a short grace);
  reopen it on Play.

#### 26. `crates/lsp` is missing from `default-members`

- Where: [`Cargo.toml`](../Cargo.toml) (from `0c67045`/`35e68b9`).
- Effect: its tests don't run under a plain `cargo test`.
- Fix: add it.

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
27. **Page frames can tear; the doc comment says otherwise.** `SharedFrame`
    ([`web/src/client.rs:73`](../crates/web/src/client.rs)) claims the worst case
    is a newer frame, but the two slots ([`web/src/frame.rs:50`](../crates/web/src/frame.rs))
    have no handshake: the host can rewrite a slot while the app uploads it (a
    cross-process data race behind `&[u8]`). Fix: a seqlock, or a third slot
    plus an ack.
28. **Frame files are world-readable on Linux and leak on a crash.** `grow`
    ([`frame.rs:78`](../crates/web/src/frame.rs)) creates them `0644` in the
    shared temp dir, follows symlinks, and doesn't use `create_new`; a crashed
    host leaves them behind. Fix: `0600` + `create_new`, and remove stale ones
    when the host starts.
29. **Cmd+L.** Configs written before the `web.focus_url` binding have no Cmd+L,
    and `page_chord` ([`plugins/web/mod.rs:556`](../crates/app/src/plugins/web/mod.rs))
    doesn't handle it, so it does nothing. With the binding, Cmd+L while editing
    the URL calls `edit_url` (`:472`) and discards the typed text. Fix: handle
    Cmd+L in `page_chord` / `url_key` (`:530`) as a fallback, and reselect when
    already editing.
30. **`web.new` reports success when no host can start** ([`plugins/web/mod.rs:165`](../crates/app/src/plugins/web/mod.rs)).
31. **Popups load in place and lose `window.opener`** (`on_before_popup`,
    [`browser.rs:559`](../crates/web-host/src/browser.rs)), which breaks OAuth and
    payment popup flows. Decision needed: open them as a new tab in the pane?
32. **Transparent edges darken (halos).** `from_rgba` / `halve`
    ([`plugin/paint.rs:192`](../crates/app/src/plugin/paint.rs)) average straight
    alpha, and the GPU filters it and blends with `ALPHA_BLENDING`
    ([`images.rs:117`](../crates/app/src/images.rs), shader `:400`). Fix:
    premultiply in `from_rgba` before the mips and blend premultiplied; opaque
    sources are unaffected. Minor: mips average sRGB-encoded values.
33. **Autoplay can start audio in a hidden tab.** `tick`
    ([`preview.rs:293`](../crates/app/src/plugins/file_browser/preview.rs)) runs
    for hidden views, and only `paint` pauses. Decision needed: should hidden
    audio keep playing? If not, clear `autoplay_at` and pause in `View::shown`.
34. **The preview follows hover.** Moving the mouse to the preview's controls
    crosses rows, switches the subject and pauses the track; in Browse mode,
    clicking an audio row opens the editor. Decision needed: follow the
    selection (as Finder does), pin to the chosen row, or hold the subject while
    the pointer travels to the preview?
35. **Stale stamps.** DECISION.md and TODO.md stamp the preview entries
    `@0c5b05d7` and the browser entries `@e175cf0d`/`@e175cf0`, which predate the
    rebase.
36. **A page keeps its hover state when the pointer leaves for another pane.**
    `hover_view` ([`window.rs:1161`](../crates/app/src/window.rs)) sends `Move`
    only to the view under the pointer, so the web view never sends `Leave`.
37. **A page that is never painted is never hidden.** `WebView` starts with
    `sent_visible: true` ([`plugins/web/mod.rs:362`](../crates/app/src/plugins/web/mod.rs)),
    so a page opened by the agent in a background tab renders (and wakes the
    app) until it is first shown and hidden.
