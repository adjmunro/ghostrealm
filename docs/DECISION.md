# Decision.md

## Instructions

- This files is a living document (managed by YOU) of important, high-level decisions (e.g. architecture, libraries chosen etc) that affect the project.
- Append to the tail of `## Decision Registry`.
- You can use `~` around the header to ~revoke~ decisions when they are redundant or no longer relevant.

## Format

- Record the datetime and current SHA at time of writing as context for posterity.
  > *Explicitly formatted as
  `[<datetime>@<commit-sha:0..8>]` with the square brackets - the header is not an optional field.
- Each element can have multiple bullet points for each category, decision detail, and reason.

```markdown
---

[<datetime>@<commit-sha:0..8>]

- Category: <scope>
- [Status: REVOKED: <why> | SUPERSEDED BY <[<datetime>@<commit-sha:0..8>]>]
- Detail: <what-was-decided>
- Reason: <why>
```

---

## Decision Registry

---

[2026-09-14T20:12+1200@e96f61f0]

> Baseline: written before version control; dated to the project's first commit
> (`e96f61f chore(repo): initialise project baseline`).

- Category: Terminal backend / core dependency
- Status: PARTLY SUPERSEDED BY [2026-10-31T08:06+1300@e01f4b6e] (the PTY is our own code, not `portable-pty`)
- Detail: Build on `libghostty-vt` (VT engine only) + own PTY (`portable-pty`) + own wgpu glyph renderer + font/shaping stack (`cosmic-text` candidate). `libghostty-vt` gives cells; we draw pixels. Ghostling (single-file reference terminal shipped with the crate) is the renderer starting point.
- Detail: Terminal substrate walled behind a trait seam from the app core; PTY / VT / renderer / fonts each behind their own internal seam so any one can be swapped as upstream Ghostty publishes more.
- Detail: Terminals render into the same wgpu scene as chrome (no native subviews) → unified compositing, palette overlay is trivial, fully cross-platform.
- Reason: Only path that reaches the cross-platform (macOS + Linux) goal. `libghostty-vt` is documented, ABI-managed, and has ready Rust crates. `libghostty-internal` (the black-box embedder) is macOS/iOS-only, undocumented, and pre-stable — see [libghostty-findings.md](libghostty-findings.md). We accept owning the renderer because Linux is a launch goal, not a "revisit later".

---

[2026-09-30T12:52+1300@7d132a83]

- Category: Architecture / content plugins
- Detail: Every surface's content is a `View` opened by a registered `Plugin` (`crates/app/src/plugin/`). Terminal, editor, and file browser are first-party plugins on the same API (`crates/app/src/plugins/`). Contract in [PLUGINS.md](PLUGINS.md).
- Detail: In-process Rust trait objects, immediate-mode paint through `PaintCx` into one shared `Frame` (quads + text via the app's single `FontSystem`/row cache/glyph atlas). No per-plugin GPU surfaces.
- Detail: Views live in `AppState` (headless), so the agent channel and tests drive them without a window. Effects outside a view go through a request queue the app applies after the call (views can't mutate the tree while borrowed from it).
- Detail: Input order: overlays → app chords (Cmd+, and `[keybindings]`) → focused view. Unhandled keys are dropped, never forwarded to another view.
- Detail: Named `Plugin`/`View`, not "panel": the glossary already uses Panel for a pane.
- Detail: The API stays inside the app crate for now; `Plugin::register_commands` takes `Registry<AppState>`, which would be a cycle in a separate crate.
- Reason: build new kinds (Git GUI, web browser, previews) as uniform modules that obey the same perf and input rules as the terminal. It also shrinks window.rs to layout/chrome/routing, and makes content testable headless.

---

[2026-10-05T20:43+1300@7654ce1]

- Category: Architecture / language services
- Detail: Editor language features speak the Language Server Protocol, first-party ones included. `crates/lsp`: a first-party server implements `LanguageServer` (name, capabilities, a hook per feature) and `serve` runs the protocol around it; the editor holds one `Client` per open document, backed by a first-party server on an in-process thread (same LSP messages over channels) or an external server process on stdio.
- Detail: A service runs only while its document is open: dropping the `Client` sends `shutdown`/`exit` (a process that lingers is killed).
- Detail: `lsp-types` for the protocol types, `lsp-server` (rust-analyzer's) for framing and the handshake.
- Detail: First-party servers can also run standalone (`ghostrealm lsp <name>`), so other editors can use them and tests drive them as external processes.
- Reason: one interface for our own small servers and existing ones (add a Kotlin server rather than build Kotlin support); in-process keeps first-party servers cheap; standard messages keep them honest and reusable.

---

[2026-10-05T20:43+1300@7654ce1]

- Category: Config
- Detail: The config file lists only what the user sets. Every option's type, range/values, default, and description live in an option catalogue (`core::config::catalogue`): the app's sections plus each plugin's (`Plugin::config_sections`).
- Detail: One checker serves the loader and the config language server. The loader drops or clamps each flagged entry instead of rejecting the file, and keeps the running config when the file doesn't parse; the editor shows the same findings as diagnostics, and the catalogue's docs on hover.
- Detail: `[keybindings]` binds only the commands it lists. The default chords exist only as the text of a new config file.
- Detail: No backfill/doctor pass and no generated option or keybinding listings in the file.
- Reason: the file had outgrown listing every option; hover docs and checking keep it discoverable and safe without the clutter.

---

[2026-10-05T21:13+1300@04794e15]

- Category: Web surface / embedded browser
- Status: PARTLY SUPERSEDED BY [2026-10-13T20:40+1300@e534aa0] (pop-up handling), [2026-10-31T15:27+1300@00d7e28d] (building without CEF)
- Detail: Web pages render via CEF (`cef` crate, prebuilt CEF binary distribution) in a separate host process (`crates/web-host`), off-screen, with software compositing. Frames reach the app through shared-memory files and are drawn as textures in the app's own wgpu scene; input and commands go over a JSON-line pipe (`crates/web`).
- Detail: One page per web surface — no in-browser tabs; popups and new-window links open in place. The pane/tab system provides tabs.
- Detail: The app side is the `web` plugin (`crates/app/src/plugins/web/`). It draws pages through `PaintCx::image` and takes keys through `View::raw_key` (presses and releases with the physical key), both added to the plugin API for it.
- Detail: The host crate is excluded from the workspace's `default-members`; the app builds and tests without CEF and reports a missing host in the pane.
- Reason: Process isolation keeps a renderer crash or hang from taking down the multiplexer and its shells, and keeps Chromium's NSApplication requirements and ~250 MB of binaries out of the main binary. Off-screen rendering keeps "unified compositing; no native subviews" and cross-platform.
- Reason: Software compositing because frames are consumed as CPU pixels anyway (GPU compositing adds a readback), and it paints without a window server.

---

[2026-10-05T20:35+1300@3f581979]

- Category: Dependencies / media preview
- Detail: Images decode with `image` (still formats only: bmp, gif, ico, jpeg, png, tiff, webp; no rayon). Audio plays through `rodio` (cpal output + symphonia decoders: flac, mp3, mp4/aac, vorbis, wav; no recording).
- Detail: Both sit behind traits of our own in `crate::media` (`ImageDecoder`, `AudioBackend`); a module in the app crate rather than a crate, as only the app uses it and it holds no app types, so splitting it out later is a move. Nothing outside `media::image::image_crate` and `media::audio::rodio_backend` names `image` or `rodio`.
- Detail: Images reach the GPU through `PaintCx::image` (the textured-quad path web pages draw through), not through a per-view GPU surface.
- Detail: Workspace `rust-version` is 1.88 (`image` needs it; `wgpu` already needed 1.87).
- Reason: pure-Rust decoders keep the build free of system media libraries on macOS. On Linux, cpal needs the ALSA development headers (`libasound2-dev`) to build.

---

[2026-10-13T19:17+1300@b804592e]

- Category: Web surface / security
- Status: SUPERSEDED BY [2026-10-13T20:40+1300@e534aa0] (the sandbox is to be enabled)
- Detail: The web host runs Chromium with its sandbox off for every process (`no_sandbox: 1` in `crates/web-host/src/main.rs`). A renderer exploit runs with the user's rights: their files, SSH keys, and the app's shells.
- Detail: Enabling it needs `cef_sandbox` linked into the macOS helper bundles, and Chromium's setuid or namespace sandbox on Linux.
- Reason: recorded so the trade-off is visible; whether to ship the browser before sandboxing it is not yet decided.

---

[2026-10-13T19:46+1300@45031372]

- Category: Config / keybindings
- Detail: A missing or empty `[keybindings]` section binds nothing, deliberately. There is no fallback to built-in defaults: every binding is explicit in the user's file.
- Detail: Config files generated before this carry the comment "If the whole section is empty, the built-in defaults apply"; it is no longer true (relevant to a future migration note).
- Detail: Recommended bindings are kept as a list per command and added to the user's file only on request (TODO.md), never automatically.
- Reason: the user's file is the only source of bindings; nothing is bound implicitly.

---

[2026-10-13T20:40+1300@e534aa0]

- Category: Web surface / security
- Detail: Chromium's sandbox is enabled for every Chromium child process (renderer, GPU, utility); the browser process is never sandboxed. There is no runtime flag to turn it off; add one only if debugging ever needs it.
- Detail: On macOS each helper process enters the sandbox (`libcef_sandbox.dylib`, `cef::sandbox::Sandbox`) before loading the CEF framework. On Linux Chromium uses its own namespace sandbox, or the setuid `chrome-sandbox` beside the host when user namespaces are unavailable; TODO.md tracks verifying it ("Web — verify the Chromium sandbox on Linux").
- Reason: a renderer exploit otherwise runs with the user's rights (files, SSH keys, the app's shells), and the agent-driven browser loads arbitrary pages. Owner, verbatim: "Definitely, I think, turn on the Chromium sandbox. I'm not even sure if we should have the ability to turn it off. If we need to turn it off in the future, maybe we can just program it to be able to turn off then. I don't think we need to have a flag or anything for this. No real advantage to turning it off, except for maybe debugging."

---

[2026-10-13T20:40+1300@e534aa0]

- Category: Web surface / profiles
- Detail: The web profile lives in the platform's application-data directory (Application Support on macOS), not in the cache directory: cookies and logins are data, not a cache.
- Detail: Profile model (design, not yet built; TODO.md "Web — profiles"): a profile picker in the browser and the GUI; human-only profiles that agents cannot reach structurally (no cookies or tokens exposed, enforced by the host and agent channel, not by instructions); agent profiles shared between agents; a per-profile scope, set by the agent or the human, of private to one workspace or shared across workspaces.
- Reason: multiple profiles are available anyway; separating human from agent state gives the isolation. Owner, verbatim: "We want this kind of isolation built in and structural, not markdown prayers or rules."
- Proposal (not decided; a consequence of the profile lock): Chromium locks a profile to one process, so the host would be owned by the running app and agents would connect through it (TODO.md "Agent channel → the running app"), rather than each process spawning its own host.

---

[2026-10-13T20:40+1300@e534aa0]

- Category: UX principle / agent-operated surfaces
- Detail: Strong preference: agent-operated browser or emulator work happens in the background and does not refocus or distract the human. If it can be done in the background, it is.
- Detail: Web pop-ups: when a human triggers one, it opens as a floating pop-up window above the whole app while the originating tab stays alive (fallback: a new tab), so `window.opener` and OAuth/payment flows work. When an agent triggers one, it opens hidden, in the background. Pop-ups do not open in place.
- Detail: "Hidden" means off screen, not hidden from Chromium: an agent's page off screen (its pop-up, or any page opened without a window) stays visible to Chromium and paints at `UNSEEN_FRAME_RATE` (10 fps), so its renderer keeps foreground priority; a person's page off screen is hidden from Chromium (Web surface / pop-ups, 2026-10-23).
- Reason: owner, verbatim: "it's really distracting when the agent keeps on refocusing on a browser to do things or an emulator to do things while I'm trying to do things as a human. So if you can do things in the background, then that's definitely a preference for agent operated stuff."

---

[2026-10-17T07:53+1300@d3b487aa]

- Category: Architecture / files and editing
- Detail: Shape S1 from GitHub issue #1: one Files surface (plugin `files`) owns a file sidebar (`FileBrowserView`, `Mode::Sidebar`), its own top row (sidebar button, file tabs), and its documents as child views (`EditorView`, `MediaView`) under a document header. A pane holding only it autohides its tab strip.
- Detail: A file opened from a Files surface's own sidebar opens in that surface. Any other open (the palette, a link, `file.open`, the agent) brings into view a Files surface of the workspace with a tab of the file (that tab shown), else goes to the focused Files surface, else the workspace's last focused one, else a new one. The editor and file browser are no longer surfaces of their own; their command ids and config sections stay.
- Detail: The file browser shows no preview beside its list: images and audio are documents (`MediaView`), and text opens in the editor.
- Detail: Closing a Files surface's last document leaves the surface showing its sidebar; closing that closes the surface.
- Reason: owner decision on issue #1 (clone the Claude Code app's file panel); alternatives were per-pane file tabs with a window-level sidebar (S2) and a per-workspace explorer (S3).

---

[2026-10-17T08:49+1300@338dc016]

- Category: UX / files and tabs
- Detail: A Files surface shows a file tab per document. A single click in its sidebar opens the file in the preview tab (italic), whose document the next single click replaces; editing it, a double click on it or its file, or an open from anywhere else keeps it. Every other open gets a tab of its own, so an open never replaces a document with unsaved changes and never asks.
- Detail: Italic means "preview tab". An unsaved document shows a dot in its tab's close slot, × while the tab is hovered; pane tabs (htabs) do the same, and the document header shows the dot after the name. Changed on disk under unsaved edits stays an amber title.
- Detail: Closing the shown file tab shows its right neighbour, else its left.
- Reason: owner decisions on issue #1 (clone the Claude Code app's file panel; unsaved indicator, answered 2026-10-16).

---

[2026-10-17T18:23+1300@5661e9a6]

- Category: Files surface / search
- Detail: The File sidebar's filter box has two modes: plain text filters names (smart case; `/`, `~`, `.` start a path to complete), and `?` first searches file contents. Search runs `core::search` on a worker over the files the sidebar lists: `.gitignore`, its hidden/ignored toggles, and its git filter (modified/untracked; the search waits for the status, and holds those files to the same ignore files and toggles; global git excludes aside). Links to files are searched; links to folders aren't followed. Binary, non-UTF-8 (the editor can't open them), and >1 MB files are skipped. Caps on lines per file, files, matches, and entries walked; a newer search cancels the older by generation.
- Detail: Search is smart case unless "match case" is on (in regex mode only literal uppercase counts, as in ripgrep: not `\W` or `\p{Lu}`). "Whole word" follows VS Code: an end of the match that is a word character must have no word character beside it, so `foo(` matches in `foo(x)` and `-x` in `a-x`. Matching one line may take only so long (`LINE_TIME_BUDGET`, 1 s: some regexes rescan the line from each start); past it the line's matching stops, the file stays listed even with no match found, and its count shows as a floor (`12+`, `0+`). A plain query is literal; "regex" uses the `regex` crate's syntax.
- Detail: `regex` and `regex-syntax` are direct dependencies of core (already built through `ignore`).
- Reason: owner request on issue #1 (the two-mode box and its toggles, cloned from the Claude Code app).

---

[2026-10-17T23:58+1300@dd14e905]

- Category: UX / files and editing
- Detail: A document has three view modes, Preview (rendered, read-only), Integrated (source with formatting, editable), and Source (plain, editable), stepped by one header button (Preview → Integrated → Source) whose icon shows the current mode; the `⋯` menu and `editor.cycle_view_mode` do the same. A language offers only the modes it supports: Markdown all three, other highlighted languages Integrated and Source, others Source. Markdown opens in Integrated.
- Detail: Preview body text is serif (`[editor] preview_font`, else New York / Georgia / fontconfig's serif) at 1.15 times the editor's size; code stays monospace.
- Detail: Markdown parses with `pulldown-cmark` (CommonMark + GFM tables, task lists, strikethrough; no default features) into a flat block list; inline below 64 KiB, else on a worker.
- Detail: A Preview link to a path (or a `file:` URL's path) opens in a Files surface, never in another program. Only `http`, `https`, and `mailto` URLs go to the system's handler (`Request::OpenUrl`), and `AppState::open_url` enforces the same list for every caller; any other scheme is not opened, and the link's hover says so.
- Reason (links): `open`/`xdg-open` runs whatever a scheme maps to (an app bundle or script by `file:`, `smb:` mounts, app schemes), so a document must not choose it.
- Reason: owner decisions on issue #1 (one cycling button like Claude's eye icon; the third, Integrated mode; serif body; pulldown-cmark approved).

---

[2026-10-18T05:18+1300@e3a7c9e]

- Category: UX / document header
- Detail: A document header's `⋯` menu lists every option of its document. `[editor] header_icons`, an ordered list of option ids (`statistic`, `autosave`, `soft_wrap`, `view_mode`, `autoplay`), picks which also show as icons left of `⋯`, in that order; a document shows the icons of the listed options it has. Default: `["statistic", "autosave", "soft_wrap", "view_mode"]`.
- Detail: Named for the document header (GLOSSARY.md), not "ribbon". An unknown or repeated id is a config warning at that entry and is dropped; the rest apply.
- Reason: owner, verbatim (GitHub issue #1): "I like the three dots menu … Perhaps we can use the three dots to show all the options (including softwrap and autosave) and then the config toml can specify which ones we want to actually show as icons in the ribbon as well, outside of the triple dot submenu."

---

[2026-10-23T07:51+1300@290d752]

- Category: Web surface / security
- Detail: `web.eval` runs the agent's script in an isolated JavaScript world (as Chrome extension content scripts do), made per document through the browser's DevTools session (`Page.createIsolatedWorld`, then `Runtime.evaluate` addressed by the world's `uniqueContextId`). The world shares the page's DOM but not its scripts' globals or prototypes, so the page cannot change how the script runs or how its value is encoded.
- Detail: Consequence: an eval sees the DOM, not page JavaScript globals (a page's `window.app` state is out of reach). Its own globals persist for the document. What it reads from the DOM is still page content. A main-world eval is not offered (TODO.md "Web — opt-in main-world eval").
- Reason: agents drive the browser over arbitrary pages (docs, GitHub, search results), so a page must not be able to lie to the agent about what it finds. Owner, verbatim: "Okay, it sounds like we need to put this isolated world thing on the to-do as a fairly high priority for the sake of security. High priority in terms of web stuff, although maybe some quality of life stuff will try and come before that. Mainly because agents can control the browsers and therefore there is clearly a prompt injection risk or a malicious use case risk. Having said that, we are most likely going to be running localhost stuff when we're doing stuff in the browser, right? Most of the time? Maybe not. I'm not sure, actually."

---

[2026-10-23T09:43+1300@5a747143]

- Category: Web surface / pop-ups
- Status: PARTLY SUPERSEDED BY [2026-10-23T21:13+1300@381bc15d] (a tab whose opener went first; any web tab closing itself)
- Detail: A pop-up (`window.open`, a link to a new window) is a page of its own in the host, so it keeps `window.opener`. Who opened it, and how the page asked for it (CEF's window disposition), decide where it shows: a person's pop-up window (`NEW_POPUP`: `window.open` with a size or position, as OAuth libraries call it) floats over its opener's page (centred, at the size it asked for, a close button in its header; the opener stays live and takes the keyboard back on a press, or when the pop-up goes unless another still floats, which then takes it); a person's new tab or window (`NEW_FOREGROUND_TAB`: a `target=_blank` link, `window.open` with no features; `NEW_BACKGROUND_TAB`; `NEW_WINDOW`) opens as a web tab right after its opener's in the same pane, shown unless it is a background tab, and outlives its opener; an agent's pop-up of any kind is a hidden surface (in no pane, never painted or focused) that `web.*` commands and `get surface` reach by id and `get tree` lists. A floating or hidden pop-up goes when it closes itself (`window.close()`) or its opener closes; such a tab goes when it closes itself.
- Detail: Rule: a pop-up is a person's when a mouse press or key from the pane reached its opener within the last 5 s (Chromium's user-activation lifespan); otherwise it is an agent's: the activation that opened it came from a gesture eval, which opened it or let the page's own script do so. When a person and an agent act on one page within 5 s, the pop-up shows (the visible failure).
- Detail: An eval runs as a user gesture only when asked (`web.eval` `gesture: true`). The activation is the frame's, not the isolated world's: the page's own scripts can spend it (write the clipboard, open pop-ups) and see it (`navigator.userActivation`), so a read-only eval never hands it over.
- Detail: A page has at most four pop-ups open at once (floating or hidden, a person's and an agent's together; a person's tabs don't count): the host refuses more (`window.open` returns `null`), and the opener's `get surface` says how many were refused since it last loaded. A refusal a person's input caused (by the person/agent pop-up rule) also shows an info toast on the pane ("Pop-up blocked: too many pop-ups open (max 4)"); an agent's shows none, since the agent reads the count. Each pop-up is a renderer process.
- Detail: `web.close` closes a web surface by id, a hidden pop-up included. While a floating pop-up has the keyboard, Close (Cmd+W) closes it rather than the tab, the keyboard going to the next floating pop-up or else the page; editing the URL (Cmd+L) takes the keyboard back to the page's URL bar.
- Detail: An agent's pop-up, hidden or not, stays visible to Chromium off screen at `UNSEEN_FRAME_RATE` (10 fps instead of 60), as does any page an agent opened without a window: Chromium runs a hidden page's renderer at background priority, which a loaded machine starves for seconds per DevTools step, past the agent's reply wait. A person's page off screen (a background tab, a pop-up not yet shown) is hidden from Chromium and doesn't paint. Frames from a page off screen don't redraw the window.
- Detail: The floating pop-up is bounded by its opener's page, not the whole window: a view receives input only inside its own rect, and floating over other panes would need the window to route the pointer to a view outside its pane.
- Reason: OAuth and payment pop-ups need `window.opener`; agent-operated browser activity runs in the background (UX principle / agent-operated surfaces).

---

[2026-10-23T21:13+1300@381bc15d]

- Category: Web surface / pop-ups
- Detail: A person's new tab whose opener (a tab, or a pop-up floating over one) goes before the app places the tab still outlives it: it opens in the focused pane, after its active surface, shown unless it is a background tab. The window is a few milliseconds after the click.
- Detail: Any web tab, one the app created included, goes when its page closes itself (`window.close()`), once the tabs that page opened are placed.
- Reason: a person who opens a tab and closes its opener at once still asked for the tab; a page that closed itself leaves nothing to show or drive.

---

[2026-10-24T02:09+1300@08234dd1]

- Category: Web surface / pop-ups
- Detail: A link clicked with Cmd (Ctrl off macOS) or the middle button opens as a background tab, with Shift too as a foreground tab, and with Shift alone (a new window) as a foreground tab, as a person's new-window pop-up already does; a `target=_blank` link clicked so does the same. Chromium sends these to `OnOpenURLFromTab`, not `OnBeforePopup`. The host cancels the in-place load and reopens the link with `window.open(url, '_blank', 'noopener')` (`noopener,noreferrer` unless a link to the URL is found, open shadow roots included, and none withholds the referrer with `rel=noreferrer` or a referrer policy of its own other than `unsafe-url`), run as a gesture eval in the page's isolated world (unseen by its scripts), and places what `OnBeforePopup` gets under the pop-up rules with the click's kind. The eval runs only in the document the click was in: the host counts the page's main-frame commits and drops a link's eval, before it runs or when retried after its world went with its document, once another document has committed, so another document's sandbox flags, referrer and forms never apply and it gets no gesture. The script also does nothing at an address of another origin (a commit the host hasn't heard of yet). In a sandboxed document (`allow-popups`) the tab is sandboxed too; without `allow-popups` nothing opens. Local files (`file:`) work as other pages do. A person's (their input within 5 s of the click) is a tab beside its opener that outlives it and isn't counted toward the four-pop-up cap; an agent's (a gesture eval dispatching a modified click) is a hidden surface that counts, is refused past the cap (the opener stays put), and closes with its opener.
- Detail: The page loads as one its opener asked for: Chromium's referrer policy, `Sec-Fetch-Site` and `SameSite` cookie rules, and the opener's sandbox flags all apply (a `SameSite=Strict` cookie doesn't go cross-site). It has no `window.opener`.
- Detail: A page's script can still turn a person's press into a tab, by dispatching a modified click on a link (Chromium makes it a foreground tab), as it can with `window.open`. One press opens one such tab: Chromium spends its activation on the first.
- Detail: A form that posts to the URL is submitted again in place, with `requestSubmit`, so its data reaches the server: the form whose submit button has the focus (a Shift- or Cmd-click on it), or after Enter (Shift+Enter in a field) the form whose field has it, with its first submit button as Enter would. Its submit handlers run a second time; a disabled submit button (a page guarding against a second submit) submits nothing. CEF's own in-place load drops a form's body, and `window.open` can't post. A posting form detected no other way opens its URL in a new tab as a GET, without the data.
- Detail: Only `http` and `https` links in the main frame open so. Other schemes that arrive (`about:blank`, `blob:`, `mailto:`) load in place, as a plain click on them does; Chromium refuses `file:`, `data:` and `chrome:` links and runs `javascript:` ones before this. A link in a frame opens nothing: the eval reaches only the main frame, whose sandbox flags may be laxer than the frame's. A same-document link (`#frag`) opens its page in the new tab, as browsers do.
- Detail: A modified click without user activation (a page's script unprompted, an eval without `gesture`) opens nothing and loads nothing, as Chromium blocks a new window without activation. Alt-click (a download in browsers) and, on macOS, Ctrl-click (a context menu) reach neither callback and do nothing.
- Reason: browsers open these in a new tab or window, and the app shows pages in its own tabs rather than windows. A page made by the host itself would load its URL as a typed address does (`Sec-Fetch-Site: none`, `Strict` cookies sent, no referrer), letting any page get a cross-site request past `SameSite=Strict` out of a person's click. Deciding in `OnBeforeBrowse` (which sees a form's method) would let the in-place load start, which runs the opener's `beforeunload`; a page that asks there (an unsaved draft) then cancels the load, and the click would open nothing.

---

[2026-10-24T03:20+1300@8b75dfd]

- Category: Web surface / host lifecycle
- Detail: The app hangs up on the web host (stdin EOF; its reaper kills it after 5 s) once no web surface has been left for 30 s, and spawns a new one when a web surface next needs it. Not configurable (`IDLE_EXIT` in the web plugin).
- Detail: A web surface is any web view the app holds, whether or not its page was made in the host yet: a tab in any workspace, shown or not; a hidden pop-up an agent opened; a pop-up floating over its opener; a pop-up its opener took that the app hasn't placed; and a person's tab whose opener went before the app placed it. An eval or text read holds its view and blocks the UI thread, so it never sees the host go.
- Detail: The 30 s count from when the event loop (or the agent channel) first sees no web surface; one opening meanwhile stops the count, which starts afresh when the next goes. The GUI ticks on every pass of its event loop and wakes for the deadline (`AppState::next_web_deadline`), as for the inbox dwell.
- Detail: No host is spawned until the one hung up on has gone (its process reaped, not just its stdout closed): it may hold the profile's lock until then, and a second host would report the profile in use. In the GUI a web surface opened meanwhile shows a blank page, `web.new` succeeds, and an address typed is its first; the old host's reaper wakes the event loop and the view's timer spawns the new host. Without a window (`ghostrealm agent`) the spawn waits for the old host to go (the reaper's kill bounds it).
- Detail: `ghostrealm agent` checks the idle exit before and after each request, and at the deadline while it waits for the next: it reads requests on a thread, one line each time it asks, so an idle host there goes on time.
- Detail: An idle exit is not a failure: no error shown, no back-off, the failure count untouched. A host that exited or failed by itself is handled as before (its reason shown, back-off), also when it did so with no web surface left.
- Reason: an idle Chromium browser and GPU process hold a few hundred MB. 30 s keeps closing the last tab and opening another from paying for a host start.

---

[2026-10-26T22:13+1300@462d3daa]

- Category: UI / errors and notices
- Detail: A failure in a form shows inline and blocks confirming until the input changes; Cancel always works. Forms: the floating picker (Save As, Save Copy As, Open; a failed Replace too) shows the error under its listing, the name field outlined red; the command palette's argument prompt (Rename, New File, New Folder; a value that doesn't parse) shows it under the input.
- Detail: Language-service diagnostics are wavy underlines under the range: red errors, yellow warnings, blue info and hints.
- Detail: Everything else is a toast: blue info, green success, orange warning, red error. One tied to a pane shows across the top of that pane below its tab strip; one about the whole app across the workspace area (window minus the sidebar) below the top row's tab strips.
- Detail: Errors and warnings stay until dismissed (`×`, a choice, or `toast.dismiss`); info and success hide after 4 s on screen. One window-wide FIFO queue shows its head only, with "+N" for the rest. A toast with a key (`config`, `preview_font`, `save:<path>`, `web:<page>`, `web-popups:<page>`, `terminal-snapshot:<n>`) replaces the queued one with that key, so repeats don't stack. A web page's navigation failures (address, Back, Forward, Reload) share `web:<page>`, so each replaces the last, or closing the page dismisses it. A web pane's refused pop-ups have `web-popups:<page>` (the pane's page, also for a pop-up floating over it), apart from `web:<page>` so the note never replaces a failure; closing the page dismisses it. A failed save and a failed autosave of one file share `save:<path>`; a successful write of that file dismisses it, and its "Save As…" choice leaves it queued until then. Each terminal has its own `terminal-snapshot:<n>`: the first of a run of failed snapshots raises it (repeats within the run don't), a later run replaces it, and the next good snapshot or closing the terminal dismisses it.
- Detail: A toast draws over the palette, picker and confirmation, its clock running as usual, and takes the pointer where it covers them: nothing of the modal under it hovers or takes a press. Its × works throughout; its choices show inert while the picker or a confirmation is open (a choice would replace the picker, or act under an unanswered question), and a choice closes the palette first. A menu draws over the toast and owns the pointer, the toast included. A right press on a menu only closes it, whatever is under it; one on the toast off any menu closes a menu and reaches nothing beneath the toast.
- Detail: Every error and warning toast's message also goes to stderr. Failures of commands an agent runs go back to the agent in its reply; web host failures stay inline in the web pane.
- Detail: A Save As or Save Copy As picker whose document has closed closes, with an app-wide error toast: no path in it can save.
- Detail: A command picked from the palette that doesn't apply where the focus is shows an info toast; from a chord it goes to stderr alone, since a chord meets that wherever the focus is.
- Reason: the owner's answer: "If the error is from a form, such as the Save As interface, then it should be inline and prevent progression (but not cancellation) until resolve. For code/text in the editor, we should use a red squiggly line under the affected area (or yellow if warning). For everything else, can we add blue(info)/green(succes)/orange(warning)/red(error) toasts, in the top of the affected panel. If it's an app issue not a panel issue, then make the width of the toast full-width excluding the vtab sidebar. Less important / non-actionable toasts, then can autohide. For important ones a human should read, keep it there until they click the 'x'/'dismiss'/other choices. That probably also means FIFO queuing them to ensure all errors are read and acknowledged, showing only 1 issue at a time."

---

[2026-10-29T21:21+1300@be5bd54a]

- Category: UX / files and editing
- Detail: `[headings]` sets each heading level's size (`h1_scale`…`h6_scale`; default 2.0, 1.6, 1.35, 1.2, 1.1, 1.0) and the space above it (`h1_space`…`h6_space`; default 1.6, 1.4, 1.2, 1.1, 1.0, 1.0), both in ems of the view's body text (Preview's is 1.15 times the editor's). Integrated and Preview read the same values; Preview's first block keeps its 0.5 em page padding.
- Detail: In Integrated the space sits above a heading's first row, except on the view's top line, so a heading scrolled or jumped to starts at the top.
- Reason: owner request: more space above each heading and larger headings in Integrated, so they read as sections; sizes in the settings TOML.

---

[2026-10-31T08:06+1300@e01f4b6e]

- Category: Terminal backend / PTY
- Detail: `crates/terminal-ghostty/src/pty.rs` opens the PTY and spawns the shell itself; `portable-pty` is no longer a dependency. The master opens with `posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC)` (or, if that fails with `EINVAL`, since POSIX leaves `O_CLOEXEC` there unspecified, by opening `/dev/ptmx` through std), then `grantpt`, `unlockpt`, `ptsname_r`, and the slave opens through std (always `O_CLOEXEC`), so both are close-on-exec from the start on macOS and Linux and no spawn lock is needed.
- Detail: The shell is a std `Command` with the slave as its stdio. Its `pre_exec` resets six signal dispositions, calls `setsid` and `ioctl(TIOCSCTTY)`, nothing else; `ghostrealm_process::inherit_stdio_only` keeps every other descriptor from it. The child's exit status and reaping are covered by the pane-close entry below.
- Detail: The crate builds on unix only (`compile_error!` elsewhere); Windows is out of scope (GOAL.md).
- Reason: `portable-pty` 0.9 marks `openpty`'s descriptors close-on-exec only after it returns, so a child another thread spawns meanwhile inherits the slave. Its `pre_exec` (`close_random_fds`) allocates after fork, which can deadlock on a lock another thread held at fork, and closes std's exec-error pipe, so a shell that fails to exec reads as one that exited. Neither can be changed from outside the crate, and the rest of it (a command builder, resize, the foreground group) is a few lines of `libc`.

---

[2026-10-31T09:10+1300@dbefd796]

- Category: Terminal backend / PTY
- Detail: Closing a pane hangs up its PTY: the worker drops the writer (a newline and the EOF character), stops the reader and closes the master, so the child gets EOF and SIGHUP from its terminal.
- Detail: A reaper thread reaps the child once it exits. If it is still unreaped 2 s after the drop, SIGHUP goes to the child's process group and to the PTY's foreground group (only if that group is still in the child's session); 1 s later, SIGKILL to the same groups. At 30 s the reaper gives up and leaves the pid unreaped.
- Detail: Reaping and signalling use the raw pid (`waitid(WNOWAIT)` to see an exit, `waitpid` to reap, `killpg` to signal), each under the `Shared::child` lock, whose `ChildState` is checked first. Nothing is signalled once the child is reaped, since its pid may be reused.
- Detail: Only holders of that lock reap the child. Nothing else in the process may call `wait` or `waitpid(-1)`, or set SIGCHLD to `SIG_IGN`.
- Detail: The 2 s, 1 s and 30 s timings are defaults the owner may change.
- Reason: A program that turns echo off (vim, less, top) never writes after the hangup, so a reader blocked on the master kept the PTY open; a program that ignores SIGHUP never exits. Signalling a reaped pid could reach an unrelated process that reused it.

---

[2026-10-31T09:47+1300@7e00ebd9]

- Category: Web surface / pop-ups
- Detail: A pop-up the app has taken outlives the web host that made it (a crash, a new bundle): a floating pop-up, an agent's hidden pop-up, and a tab a page opened are each made again in the next host at their last address, under the same page id, so each keeps its surface, kind, owner (person or agent) and place (its opener's page, or its pane). It has no `window.opener`. A floating one is made with its opener's page; the others on the next pump once a host runs, or when driven or painted (no host starts for them alone).
- Detail: A floating or hidden pop-up made again counts toward its opener's four pop-ups and closes with it: `create` names its opener. Past the cap the host refuses it as it refuses a new one (a person's with the info toast, an agent's counted in `get surface`) and it goes. Before anything else the app tells a new host to number its own pop-ups past the ids the app holds (`first_popup`), so no id is reused. Protocol version 7.
- Detail: One with no address, or at `about:` or `blob:`, can't be made again: what it showed came from its opener's script or from a renderer that went with the host. It goes, with a warning toast naming it and saying the web host restarted and it had no address to reopen, on its opener's pane (a tab's own pane for a tab). An agent's hidden pop-up gets one too: the person otherwise can't tell why it went.
- Detail: The pop-ups one that can't be made again opened (hidden, or floating over it, and theirs) close with it, as they would had it closed: its toast names them.
- Detail: A page the exited host reported closed, before the app saw it, stays closed: it isn't made again.
- Reason: a host crash lost every pop-up silently, an OAuth window or an agent's work among them.

---

[2026-10-31T15:27+1300@00d7e28d]

- Category: Build / web host
- Detail: The app's build script builds and bundles the web host: `crates/app/build.rs` (rules in `crates/app/build/web_host.rs`) runs a nested `cargo build` of `ghostrealm-web-host` in its own target dir (`<target>/web-host`, the app's profile), runs the bundler there, and moves the bundle beside the app binary. Plain `cargo build`, `cargo run [--release]` and `cargo test -p ghostrealm` produce it.
- Detail: Cached: the script reruns only when a host input changes (the host's path-dependency sources and manifests, the workspace manifest, lock file, Cargo config and `mise.toml`, the nested build scripts' env inputs) or the placed bundle goes; a stamp (the host, helper and bundler executables' mtimes and sizes, and a rules version) skips re-bundling.
- Detail: A failed host build fails the app build. It is skipped, with a warning and the reason in the app's "web host not found", when `GHOSTREALM_NO_WEB` is set (agents set it unless the task touches web), in rust-analyzer's build-script pass, for non-macOS and cross targets, and when the build's target dir isn't the one `cargo metadata` names (a command-line `--target-dir`).
- Detail: Placement replaces only a bundle that is ours: a real directory owned by the build's user whose `Info.plist` names `dev.ghostrealm.web-host`.
- Detail: `mise run web` is removed. `clean-stale` cleans `target/web-host` as a target dir of its own and has an opt-in `--unused-days`.
- Reason: the owner wants plain `cargo build` and `cargo run --release` to give a working browser (decisions-pending item 2, option A). The separate target dir keeps the nested build off the outer build's lock, and CEF out of the app's own units.

---

[2026-10-31T21:40+1300@63bcddb3]

- Category: Terminal / input
- Detail: On the primary screen the terminal sends the text keymap's word and line motions and deletes as the readline keys shells bind (`ESC b` / `ESC f`, `^A` / `^E`, `ESC DEL`, `ESC d`, `^U`, `^K`), and Option+Up/Down as plain arrows. Shift chords select (Shift+Arrow), drive the scrollback (Shift+Page Up/Down, Home, End) or send a bare LF (Shift+Enter).
- Detail: While the program is on the alternate screen (as last painted), no key is translated: each sends libghostty's normal encoding (Option+Left is `CSI 1;3D`); Cmd motions and deletes send nothing, like other Cmd chords besides copy and paste.
- Reason: full-screen programs (Vim, less, htop) bind their own keys and must get them as a plain terminal sends them; readline bytes there mean something else (`^A` increments a number in Vim). Owner's call (decisions-pending item 8, option B).
