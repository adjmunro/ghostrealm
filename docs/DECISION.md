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
- Status: PARTLY SUPERSEDED BY [2026-10-13T20:40+1300@e534aa0] (pop-up handling)
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
- Detail: Chromium's sandbox is enabled for every web-host process. There is no runtime flag to turn it off; add one only if debugging ever needs it.
- Detail: Needs `cef_sandbox` linked into the macOS helper bundles, and Chromium's setuid or namespace sandbox on Linux. Work tracked in TODO.md ("Web — enable the Chromium sandbox").
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
- Reason: owner, verbatim: "it's really distracting when the agent keeps on refocusing on a browser to do things or an emulator to do things while I'm trying to do things as a human. So if you can do things in the background, then that's definitely a preference for agent operated stuff."

---

[2026-10-17T07:53+1300@d3b487aa]

- Category: Architecture / files and editing
- Detail: Shape S1 from GitHub issue #1: one Files surface (plugin `files`) owns a file sidebar (`FileBrowserView`, `Mode::Sidebar`), its own top row (sidebar button; file tabs next), and its documents as child views (`EditorView`, `MediaView`) under a document header. A pane holding only it autohides its tab strip.
- Detail: A file opened from a Files surface's own sidebar opens in that surface. Any other open (the palette, a link, `file.open`, the agent) brings into view a Files surface of the workspace showing the file already, else goes to the focused Files surface, else the workspace's last focused one, else a new one. The editor and file browser are no longer surfaces of their own; their command ids and config sections stay.
- Detail: The file browser shows no preview beside its list: images and audio are documents (`MediaView`), and text opens in the editor.
- Detail: Closing a Files surface's last document leaves the surface showing its sidebar; closing that closes the surface.
- Reason: owner decision on issue #1 (clone the Claude Code app's file panel); alternatives were per-pane file tabs with a window-level sidebar (S2) and a per-workspace explorer (S3).
