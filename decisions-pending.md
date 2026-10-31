# Decisions pending

Open items from `docs/TODO.md`, `docs/IDEAS.md`, the two stack-review findings files, and the "decision needed" markers in `docs/DECISION.md`, `docs/SPEC.md`, and `docs/GOAL.md`, as of the commit that last changed this file. Each one needs a call only you can make. The Files surface work (issue #1, file tabs, the unsaved dot) is decided and left out. Fill in each **Your answer** block. Sections are ordered by what they block and what they risk.

3. **Agent channel into the running app**: which processes may connect, and what may each one reach?
4. **Web profiles, concrete model**: which profiles exist by default, who creates them, what scope they default to, and who owns the host?
5. **Web downloads, uploads, and JS dialogs**: what may an agent-driven page do with files and dialogs?
6. **Syntax highlighting direction**: tree-sitter, hand-written lexers, or LSP semantic tokens?
7. **Dependency policy**: what must a new dependency clear, and do we start replacing existing ones?
9. **Config write-back and a machine-local state file**: may the app edit `config.toml`, and where does per-machine state go?
10. **Settings regenerate / cleanup / reset**: which of these commands, and how do they behave?
11. **Video playback**: which decoder stack, if any?
13. **Dragging a tab to split**: leaf-only drops, or workspace-edge drops as well?
14. **Undo scope**: what should undo cover beyond what it does today?
15. **Markdown syntax for colour and embed size**: which dialect do we extend?
16. **Markdown features that reach outside the file**: should link checks use the network, and what does the run button do?
17. **Markdown language server and formatter**: external, in-tree, or both?
18. **Indexer scope**: free text, code symbols, both, or none for now?
19. **Run Anything inside the palette**: a prefix, a fallback row, or tabs?
20. **Shortcuts sheet**: build it, where does it live, and can the text keymap be configured?
21. **Web preload / prerender default**: off, connect, or prerender?
22. **Comment mode storage**: where do comment threads live?
23. **GIT.md "committing is pre-authorised"**: is this intended?

---

## 3. Agent channel into the running app: who connects, what they reach

**Source:** docs/TODO.md entry "Agent channel → the running app (+ MCP server)" (line ~681); docs/GOAL.md "Agent control channel"; docs/IDEAS.md "hide from agent" (line ~39); TODO "Agent screenshot command" (line ~710); DECISION.md "Web surface / profiles"

**What's being decided:** the trust boundary of the socket that lets agents drive the live window: who may connect, and what each connection may see and drive.

**Background:**
- `ghostrealm agent` (`crates/app/src/agent.rs`) runs its own headless `AppState`. Inside it, `input` types into any surface by id, and `get surface` returns any surface's text, web page text included.
- The plan is a per-window socket whose path is exported to child shells, plus an MCP stdio shim.
- Any process running as you can connect to a Unix socket whose path it knows. Wired up naively, an agent in workspace A could read and type into workspace B's shells, and read pages in a human web profile. You said the human profiles must be unreachable "built in and structural, not markdown prayers or rules".
- GOAL.md already scopes the agent: "scoped to its own vertical tab plus app-wide settings". What's open is how that scope is enforced.
- On Linux, any process running as you can read another process's environment (`/proc/<pid>/environ`); on macOS that is restricted. So a token kept in an env var protects better on macOS.

**Options:**
- **A. A token per workspace.** Each shell gets the socket path plus a token bound to its workspace. The app limits every op to that workspace's surfaces and agent web profiles, plus app-wide commands, and refuses a connection without a token. The socket sits in a 0700 directory.
  - Pros: matches GOAL.md; enforced in the app.
  - Cons: work that crosses workspaces needs an explicit grant; on Linux a same-user process can borrow a token.
  - Effort: medium.
- **B. Filesystem permissions only.** Any connecting process gets full access, and a "hide from agent" command list limits what it can run.
  - Pros: simplest.
  - Cons: no workspace scope. Human-profile isolation would rest on the host refusing a short list of ops.
- **C. A, plus a prompt for escalation.** An agent asking for something outside its scope triggers an approve/deny prompt in the GUI.
  - Pros: flexible.
  - Cons: prompts are the kind of interruption you want agents to avoid; more UI.
  - Effort: medium to large.

Sub-question: should "hide from agent" be a list in `config.toml` (shareable) or a per-command flag in the registry (fixed in code)?

**Recommendation:**
- Take A.
- Surfaces in human profiles are excluded from `get surface`, `input`, `web.eval`, and screenshots, whatever the token.
- Layer a config deny list on top of the token scope.
- Add C later only if A's boundary gets in the way.

**Your answer:**
> **C, with a standing grant.** By default an agent is scoped to its own workspace (per-workspace token, enforced in the app). Reaching the rest of the app (e.g. for testing) needs human approval, given via a per-workspace toggle in the UI so it can be set once and left on for autonomous jobs.
>
> **Human-only things are structural, not rules.** Some things are hard-coded out of the command registry/MCP entirely, so they can only be driven through the UI. Human-only web profiles are among them.
>
> **Profile access lives on the profile.** Which workspaces can use a profile, and whether it's human-only or human+agent, are profile settings (see item 4).
>
> **Open risk / follow-up:** CEF stores cookies and logins on disk, readable by any same-user process (including an agent's shell). Check whether CEF uses Keychain-backed cookie encryption on macOS. Explore app-managed encryption at rest, one key per profile (not per workspace) held in the Keychain so only Ghost Realm can unlock it.

---

## 4. Web profiles, concrete model

**Source:** docs/TODO.md entry "Web — profiles: picker, human-only vs agent, per-workspace scope" (line ~1128); DECISION.md "Web surface / profiles"; docs/stack-review-findings-2.md item 19

**What's being decided:** the defaults and controls left open by the profile model you described. The principles are decided: the profile lives in Application Support, human profiles are unreachable by agents, agent profiles are shared, and each profile has a scope.

**Background:**
- The profile is still at `~/Library/Caches/ghostrealm/web` (`cache_dir()` in `crates/web-host/src/main.rs`). Moving it is decided but not yet done.
- One CEF process can serve several profiles (one request context per profile directory). A second process can't open a profile the first already holds.
- The open proposal: the running app owns the single host, and agents reach it through the socket in item 3.

**Questions and options:**
- **a. The starting set.**
  - (i) Two built-in profiles, "Human" and "Agents (shared)".
  - (ii) One human profile only; agent profiles are created on demand.
  - (iii) No built-ins; a picker on first use.
- **b. Who creates agent profiles.**
  - (i) Agents, through a command.
  - (ii) Humans only.
- **c. Default scope of a new agent profile.**
  - (i) Private to its workspace.
  - (ii) Shared across workspaces.
- **d. Picker location.** A profile chip left of the URL bar; a new web surface uses its workspace's last profile.
- **e. Host ownership.**
  - (i) The GUI owns the host, and a standalone `ghostrealm agent` spawns its own only when no GUI is running.
  - (ii) Standalone agents always get throwaway in-memory profiles.

**Recommendation:** a(i), b(i), c(i) private by default, d as described, and e(i). This ships the structural isolation first and leaves named profiles and scope editing as later work.

**Your answer:**
> **Ownership by opener.** Whoever opens a browser owns it: opened from the UI means human-only; opened over MCP means agent-owned. Agent browsers are always visible to and usable by the human (e.g. to enter login details).
>
> **Profiles:** One unified human profile shared across all workspaces. Each workspace's "view" of it is bookkeeping only (a record of which sites were used there), not a separate profile and no copying. Agents always use their own separate profile(s). An agent must never drive a pane on the human profile.
>
> **Sharing a browser with an agent:** a toggle on the pane. The pane stays in place but reloads under the agent profile (unsaved page state is lost). The first time it's enabled, show a prominent warning listing exactly what the agent gains access to. On re-enable, prompt again only if access has grown: show the new items first, with the already-granted ones greyed out underneath.
>
> **No session/cookie copying between human and agent profiles.** Copying risks token rotation logging the human out, and write-back would let an agent plant cookies in the human profile. Instead, agents get their own independent sessions.
>
> **Login handover:** the agent requests a login (command), or the human presses "take over". The pane enters human control and the app refuses every agent op aimed at it until it's handed back. Backstop: refuse agent script/eval on pages with a password field. For unattended jobs, log in to the agent profile ahead of time; if a session expires mid-job, that site pauses until a human returns.
>
> **Password managers, staged:** v1 is manual handover only. Later, optionally fill credentials from the 1Password CLI during a handover. Browser extensions are likely out: CEF only supports extensions in Chrome-style windows with Chrome UI. Apple Passwords is unverified (no general read access for third-party apps). A home-grown manager is a maybe for later (crypto is easy; sync, recovery and phishing-safe matching are the hard parts).
>
> **Follow-up (from item 3):** check CEF's on-disk cookie encryption on macOS; consider per-profile at-rest encryption with keys in the Keychain.

---

## 5. Web downloads, uploads, and JS dialogs

**Source:** docs/TODO.md entry "Web surface — follow-ups" item (3) (line ~668); DECISION.md "UX principle / agent-operated surfaces"

**What's being decided:** what happens when a page asks to download a file, upload one, or show `alert` / `confirm`, for human-driven and agent-driven pages.

**Background:**
- `crates/web-host/src` has no download or dialog handlers yet, so these use CEF's defaults or do nothing at all.
- With agents in the picture:
  - An upload dialog an agent fills in can send any of your files to a site.
  - A download an agent triggers writes an untrusted file to disk.
  - An unanswered JS dialog stalls the page and the agent with it.
- Partial mitigation: an agent running in a shell can already read your files and reach the network. Limiting the browser matters most for agents whose only tool is the agent channel or MCP.

**Options:**
- **A. Split by who triggered it.**
  - Humans get in-app dialogs: the floating picker for uploads, `~/Downloads` for downloads, a modal for alert and confirm.
  - Agents:
    - Downloads go to a per-profile folder under Application Support.
    - Uploads may only come from inside the agent's workspace root.
    - JS dialogs are auto-dismissed and reported in the agent's reply.
- **B. Like A, but agents can't download or upload at all.**
- **C. Same rules for both.** Agents may pick any file.
  - Pros: simplest.
  - Cons: the largest exfiltration channel.

**Recommendation:** A. It keeps agent work in the background, as you want, and confines agent file access to its workspace.

**Your answer:**
> **JS dialogs (agent-driven pages):** don't auto-dismiss blindly. Hold the dialog open and report its type and text to the agent, which can accept, dismiss, or enter text via a command (Playwright-style), so agents can test sites properly. On timeout with no response, dismiss so the page never stalls. Human-driven pages get normal in-app modals.
>
> **Per-site permissions system:** agent uploads and downloads are **off by default** for agent-owned and agent-shared browsers. A human grants permissions per host, granularly. Upload and download are separate permissions. Generalise to the usual browser site permissions (camera, microphone, etc.) under the same system.
>
> **Two layers:** a global list plus a per-workspace list. The workspace list overrides the global one in either direction (turn off something global allows, or turn on something global doesn't mention). Lists are human-controlled only.
>
> **Download location:** agent downloads go to `~/Downloads/Ghost Realm/<workspace>/`. Human downloads go to `~/Downloads` as normal.
>
> **Quarantine:** make sure downloads get the macOS `com.apple.quarantine` attribute (verify CEF does this; don't assume) so Gatekeeper checks on open. Note: an agent executing a file from its shell bypasses this, and that's governed by agent shell policy, not the download path.

---

## 6. Syntax highlighting direction

**Source:** docs/TODO.md entry "DISCUSSION: syntax highlighting direction" (line ~1055); "Syntax highlighting — more languages" (line ~425); "Markdown rich text — follow-ups" (Kotlin heading modes, stateful fences); "Editor — section outline" (line ~564)

**What's being decided:** how code languages get highlighted. This choice also decides whether we have a syntax tree for outlines and the "Kotlin class as H1" modes.

**Background:**
- `ghostrealm_core::highlight::ALL` holds two hand-written, line-local highlighters: TOML and Markdown.
- The `Token` vocabulary mixes generic categories (Key, String, Comment) with Markdown-only ones (Strong, Emphasis, Link, Marker). It needs splitting into syntax categories and rich-text styles under every option.
- Your languages: Kotlin, Gradle KTS, Java; then Swift and Rust; then web. You want Markdown to keep its own highlighter.
- Tree-sitter: an MIT-licensed C library plus one generated C parser per language, compiled with `cc`. It parses incrementally, and its `highlights.scm` queries give standard capture names. Grammar quality varies; Kotlin's grammar is community-maintained.

**Options:**
- **A. Tree-sitter for code languages**; Markdown and TOML stay hand-written.
  - Pros: many languages from existing grammars. The syntax tree also feeds the outline, the Kotlin heading modes, and fence highlighting.
  - Cons: C dependencies and longer builds, which cuts against "Reduce dependencies"; grammars must be pinned and updated.
  - Effort: medium for the integration, small per language after that.
- **B. Hand-written lexers per language** behind a shared keyword API.
  - Pros: no dependencies; full control.
  - Cons: every language is a project, and keyword lists drift over time. A lexer gives no structure, so class and function headings still need a parser.
  - Effort: large across five or more languages.
- **C. LSP semantic tokens first**, with a minimal lexer as fallback.
  - Pros: exact highlighting.
  - Cons: needs a server installed per language; tokens arrive late after open; Markdown fences get nothing without a server.

**Recommendation:** A. Split the `Token` vocabulary first. The heading modes and outline you want need a syntax tree, which only A provides cheaply.

**Your answer:**
> **A: tree-sitter** for code languages (via the official `tree-sitter` Rust crate plus per-language grammar crates). Markdown and TOML keep their hand-written highlighters. Split the `Token` vocabulary first. Accepted as a core dependency: stable, widely used (Neovim, Helix, Zed, GitHub), incremental and error-tolerant. The syntax tree should also feed the outline, Kotlin heading modes, fence highlighting, and later the indexer (item 18).

---

## 7. Dependency policy

**Source:** docs/TODO.md entry "Reduce dependencies" (line ~796); it touches items 6, 9, and 11, and "File and folder context menu — remaining" (Linux Trash)

**What's being decided:** the bar a new dependency must clear, and whether to start replacing existing ones.

**Background:**
- Current dependencies include `image`, `rodio`, `cef`, `lsp-types` / `lsp-server`, `crossbeam-channel`, `toml`, and `libc`.
- The media crates sit behind our own traits, so replacing one is a local change.
- Items waiting on this answer, and what each would add:

  | Item | Would add |
  |---|---|
  | 6, syntax highlighting | tree-sitter plus grammars |
  | 11, video | a video decoder |
  | Linux Trash | `trash` |
  | 9, config write-back | `toml_edit` (keeps comments; the `toml` crate drops them when it rewrites a file) |

- The project is MIT-licensed. The likely licensing conflict is ffmpeg: LGPL, or GPL with some codecs.

**Options:**
- **A. Permissive.** Take well-maintained crates freely, wrap the swappable ones behind our own traits, and do no replacement work.
- **B. Gated.** A new dependency must have an MIT/Apache/BSD/MPL-compatible license and sit behind our own trait where a swap is plausible. Anything that pulls in C/C++ or a large transitive tree needs your OK. Existing dependencies get replaced only when we touch that code anyway.
- **C. Minimal.** Write it ourselves unless the job is large (codecs, browser, VT), and actively replace small dependencies (`crossbeam-channel` → `std::sync::mpsc`, and so on).

**Recommendation:** B. It keeps licensing and build weight visible without stalling items 6, 9, and 11.

**Your answer:**
> **Dependency policy (roughly B, leaning C for small deps):**
> - Unless a dependency is truly integral (graphics stack, browser engine, VT-level jobs where we're deeply coupled), it sits behind our own trait/interface so it can be swapped.
> - Simple dependencies doing simple jobs should be replaced with our own code over time, to cut the risk surface.
> - Large jobs (codecs, browser, tree-sitter) stay as dependencies.
> - Watch the transitive tree (`cargo tree`). Anything pulling in C/C++ or a large transitive tree needs my OK.
>
> **Licence (new decision, made here):** relicense the project under the **Functional Source License (FSL)**. Anyone may read, modify, fork, and use it, including commercially at work, but not repackage or sell it as a competing product. Each version converts to a permissive licence after two years. The current MIT licence was not a deliberate choice: replace it.
>
> **Hard rule: no GPL dependencies.** GPL is incompatible with FSL. Any dependency that is GPL (or would force GPL) must be flagged to me for a licence review and never just added. LGPL is acceptable only when dynamically linked and replaceable (e.g. an LGPL-only ffmpeg build: no `--enable-gpl`, no x264/x265). Playback only needs decoders, which LGPL builds include.

---

## 9. Config write-back and a machine-local state file

**Source:** docs/TODO.md entries "Palette — a 'hide' button per command" (line ~173); "Command palette — MRU order + pinning" (line ~186); "Files surface — media follow-ups" autoplay (line ~396); "Shortcuts sheet" (line ~640); "Keybindings: recommended defaults" (line ~808); "Colour swatches in the gutter + colour picker" (line ~628); "Splits" (line ~234: ratios not saved)

**What's being decided:** whether the app may edit `config.toml` itself, and where per-machine history lives.

**Background:**
- The app writes `config.toml` only when creating it. GOAL.md calls the file "designed to be committed/synced", and DECISION.md says nothing is bound implicitly.
- Six features wait on write-back: the hide button, MRU, pins, injecting recommended bindings, the colour picker, and the reset commands (item 10).
- `config_watch.rs` already reloads the file when it changes on disk.
- The `toml` crate drops comments and reorders the file when it rewrites it. `toml_edit` doesn't, but it is a new dependency (item 7).

**Options:**
- **A. Explicit write-back, plus a state file.**
  - Write-back: only on an explicit action (hide, add binding, pick a colour), with `toml_edit`, keeping comments and order.
  - State: MRU, pins, split ratios, and window size go to `state.toml` in the platform state dir (`$XDG_STATE_HOME`, or Application Support), which is never synced.
- **B. No write-back.**
  - The app opens the config at the right spot and offers the edit as a config-server code action, and you save it yourself.
  - Pros: the file stays entirely yours.
  - Cons: every one of those features takes an extra step.
- **C. Like A, but pins go in `config.toml`**, so favourites sync across machines; MRU stays in state.

Sub-question: should the media `autoplay` button stay a per-document override (as now), or write `[file_browser] preview_autoplay`?

**Recommendation:** A, with the autoplay button kept per document. Pick C instead if you want pins on every machine.

**Your answer:**
> **Three layers, split by who writes it and whether it syncs:**
> 1. `config.toml`: shared, human-intent settings, synced across machines. This includes palette hide and pin, keybindings added from the app, colour-picker writes, and human-chosen defaults (e.g. a default 75/25 split ratio). The app may write here only on an explicit user action, using `toml_edit` so comments and order are preserved.
> 2. `config.local.toml`: human settings for this machine only, never synced. Overrides `config.toml`.
> 3. `state/` directory (platform state dir, never synced): app-generated output, one JSON file per feature (MRU, current split sizes, window size, etc.). Uses `serde_json`, which is already in the tree via the LSP crates. One file per feature avoids write contention and limits corruption to one feature.
>
> (Effectively option C extended: pins and hides sync; programmatic state stays local.)
>
> **Autoplay sub-question:** the media autoplay button sets the global default for all videos (writes `[file_browser] preview_autoplay` to `config.toml`), not a per-document override. **Follow-up:** check the provenance of the current per-document behaviour (TODO "Files surface — media follow-ups" ~line 396, and git history). Was it something I specified, or an agent choice?

---

## 10. Settings regenerate / cleanup / reset

**Source:** docs/TODO.md entry "DISCUSSION: settings regenerate / cleanup / reset-to-default" (line ~993)

**What's being decided:** which of your three ideas to build. In your words: "a regenerate settings.toml command? / a cleanup command? / or a reset to default command".

**Background:**
- The file lists only what you set, and defaults live in the option catalogue.
- DECISION.md rules out any backfill or doctor pass that rewrites the file on its own; these would be commands you run.
- All three depend on write-back (item 9).

**Options:**
- **A. Two commands.**
  - "Settings: Reset to Defaults…" opens a popup listing every non-default entry (current → default) with a checkbox each, and resets the ticked ones.
  - "Settings: Clean Up" removes unknown keys and entries equal to their default, showing the diff before writing.
  - Regenerate is "reset all" with the default chords added back. Each command writes `config.toml.bak` first.
- **B. All three as separate commands.**
- **C. No commands.** Config-server quick fixes in the editor do the job: "remove: equals default", "remove unknown key".

**Recommendation:** A. Reset with a confirmation popup is what you described, and regenerate is a case of it.

**Your answer:**
> **Parked: not a priority; revisit later.** Unsure we need these commands at all.
>
> **Key concern to resolve first (default drift):** if a shipped default changes in an update, users relying on that default, whether the key is absent from their file or was removed by a clean-up, get the behaviour changed underneath them. Any design here must prevent that rug pull, automatically and programmatically (no agent involved). Ideas to explore:
> 1. On update, auto-write the old default into the user's file for any changed default (an exception to the "no implicit rewrite" rule).
> 2. Versioned defaults: the config records a `defaults_version`. The app keeps old default sets, so users stay on their version until they opt in to upgrading, with a diff shown first.
>
> Also: "clean up" must not remove entries equal to the current default unless drift is handled.

---

## 11. Video playback

**Source:** docs/TODO.md entry "Files surface — media follow-ups" (line ~394: "an ffmpeg-class dependency decision")

**What's being decided:** whether and how a Files document plays video.

**Background:**
- Images decode with `image`, and audio plays through `rodio` / symphonia, both behind `crate::media` traits. DECISION.md chose pure-Rust decoders so the macOS build needs no system media libraries.
- Video needs a demuxer, decoders (H.264/HEVC/VP9/AV1), and audio/video sync.
- CEF's standard binary distribution leaves out the proprietary codecs (H.264, AAC), so it plays WebM but not most MP4 files.

**Options:**
- **A. ffmpeg** (`ffmpeg-next`).
  - Pros: every format.
  - Cons: needs a system or bundled ffmpeg (tens of MB) under LGPL (GPL with some codecs), which breaks the "no system media libraries" rule.
- **B. Platform decoders behind a `VideoBackend` trait.** AVFoundation/VideoToolbox on macOS (via `objc2`, hardware decode, no new libraries), GStreamer on Linux later.
  - Pros: small; fits shipping macOS first.
  - Cons: two backends; Linux needs GStreamer installed.
- **C. Play it in the web host** with `<video>`.
  - Pros: reuses the existing frame pipeline.
  - Cons: no H.264/MP4; needs the host built.
- **D. Defer video.**

**Recommendation:** D until you want video, then B.

**Your answer:**
> **A, via a user-installed ffmpeg, optional and not bundled.**
> - Ghost Realm does not bundle ffmpeg. It uses a system-installed copy, declared as an optional dependency in package managers (Homebrew, Pacman, etc.).
> - Load ffmpeg dynamically at runtime, not linked at build time. If it's missing, video is disabled and the UI shows a clear "install ffmpeg to play video" message. Images and audio keep working.
> - Licence note (see item 7): distro builds (e.g. Homebrew) are often GPL. Because we neither distribute nor statically link it, this should be acceptable, but it's a grey area. Keep it runtime-loaded and record the reasoning.
> - Standing rule: flag any future dependency that is a notably large binary for my review.

---

## 13. Dragging a tab to split

**Source:** docs/IDEAS.md "Drag a htab to re-place it / create a split" (line ~13); docs/TODO.md entry "Horizontal tabs — overflow scroll, drag-reorder, drag-to-split" (line ~239)

**What's being decided:** where a dragged tab can be dropped, including your "full-width pane across the bottom" case.

**Background:**
- The split tree (`core::tree`) is binary. A full-width pane under two side-by-side panes is a split of the root (root = top/bottom of the old root and the new pane), so the tree can express it.
- What's missing is drop zones that target an ancestor rather than a leaf.

**Options:**
- **A. Leaf zones only.** A pane's edge regions split that pane on that side, and its centre moves the tab into the pane's tab strip.
  - Pros: simple.
  - Cons: spanning splits are impossible by dragging.
- **B. A, plus workspace-edge bands.** A thin band at each edge of the workspace splits the root.
  - Pros: covers your example with one extra zone type.
- **C. B, plus ancestor zones.** Near an edge shared by several levels, nested hints let you pick which split to divide.
  - Pros: any layout.
  - Cons: busy hints; harder to hit.

**Recommendation:** B.

**Your answer:**
> **B: leaf zones plus workspace-edge bands.** Must include clear visual guides: once a drag starts, all drop zones (pane edges/centre and workspace edge bands) faintly highlight. Hovering a zone shows a translucent preview of exactly where and how big the resulting pane will be (VS Code style).

---

## 14. Undo scope

**Source:** docs/TODO.md entry on undo (line ~340: "User has further ideas on scope/behaviour — discuss before extending"); "File and folder context menu — remaining" (line ~912, file-operation undo)

**What's being decided:** what undo should cover. The TODO waits on your ideas.

**Background:**
- The editor's undo keeps up to 200 whole-buffer snapshots and merges typing and delete runs. Undoing back to the saved text clears the unsaved dot.
- Gaps: no undo in single-line fields (`LineEdit`); history is lost on Save As and reload.
- File operations have no undo. Zed has one for create, rename, move, and Trash.

**Options:**
- **A. Standard editor undo.**
  - An operation log instead of snapshots.
  - Runs break at word boundaries and pauses.
  - Undo in single-line fields.
  - History survives Save As and reload, but not a restart.
- **B. A, plus history kept across restarts** (like Vim's undofile), stored in the state dir.
- **C. A, plus an undo tree**: branching history with a browser, as in Vim or Emacs undo-tree.
- **D. File-operation undo.** A separate log; Cmd+Z in the sidebar reverses create, rename, move, and Trash.

**Recommendation:** A now, D next (getting a file back from the Trash is the main value), and B or C only if you want them. Add your own ideas below.

**Your answer:**
> 1. **Proper standard undo** (option A): an operation log, runs that break at word boundaries and pauses, and history that survives Save As and reload.
> 2. **Undo tree per open file** (option C): branching history tied to the file, not the pane, and discarded when the file closes. **No undo across restarts** (B dropped).
> 3. **File-operation undo** (option D) for create, rename, move, and Trash, with its own tree.
> 4. **Embedded undo panel:** the editor (one per open file) and the file browser each get a toggleable undo panel showing their tree, where you can walk it to undo, redo, and switch branches. Embedding avoids focus ambiguity.
> 5. **No other app-wide undo** (layout etc.) for now.
> 6. **Simple linear undo/redo built into the standard single-line text field** (`LineEdit`), used everywhere (rename, palette, URL bar, etc.). No tree.
> 7. **Standing note:** revisit undo coverage as new surfaces are added. Each new pane type should be assessed for whether it needs undo, e.g. web form interactions, or a future Git pane where undoing and redoing Git actions would be very valuable.

---

## 15. Markdown syntax for colour and embed size

**Source:** docs/TODO.md entries "Markdown: text colour and text background colour" (line ~1268: "Requires: a syntax decision") and "Markdown: embedded files" (line ~1242: "syntax to decide")

**What's being decided:** how to write text colour, background colour, and an embed's width and height. Markdown has no syntax for any of these.

**Background:**
- Mixed mode shows the markers, dimmed, at all times, so verbose syntax stays on screen.
- How other renderers handle these:

  | Renderer | Colour | Size |
  |---|---|---|
  | GitHub | Drops `style` attributes on `<span>` | Keeps `<img width height>` |
  | Obsidian | `==highlight==` for background | `![alt\|300x200](path)` |
  | Pandoc / Quarto | `[text]{style="color:red"}` | `![alt](path){width=300}` |

**Options:**
- **A. HTML.** `<span style="color:…">`, `<mark>`, and `<img src width height>`.
  - Pros: standard.
  - Cons: verbose in mixed mode; needs an HTML-subset parser; GitHub drops the colour.
- **B. Attribute braces.** `[text]{fg=red bg=yellow}` and `![alt](path){width=300}`.
  - Pros: compact; one rule for both features.
  - Cons: GitHub shows the braces literally.
- **C. Obsidian style.** `==text==` for background and `![alt|300x200](path)` for size. Size degrades cleanly on GitHub, because alt text isn't shown once the image loads.
  - Cons: no foreground colour.
- **D. Mixed.** Size from C, colour from B (`[text]{fg=… bg=…}`).

Colour values could be theme names (the terminal's ANSI palette: `red`, `bright-blue`) so they follow the theme, or hex.

**Recommendation:** D, with theme colour names and hex both accepted.

**Your answer:**
>

---

## 16. Markdown features that reach outside the file

**Source:** docs/TODO.md entry "Markdown rich text — follow-ups" (line ~453: link checks "a reachable URL needs a network decision"; line ~457: run button for shell-flavoured blocks)

**What's being decided:** (a) whether link checking makes network requests; (b) what the run button in a shell code block does.

**Background:**
- Checking a URL reveals to its host which documents you read, and it can hit rate limits.
- A README from a cloned repo is untrusted content, and a one-click run of `curl … | sh` is the risk case.

**Options for (a):**
- **A. Local only:** files and `#anchors`.
- **B. Also HEAD-request URLs** on open or save, cached, behind an opt-in `[markdown] check_urls`.
- **C. Local always, plus a "Check Links" command** that checks URLs on demand.

**Options for (b):**
- **A. Run immediately** in a new terminal tab in the file's directory.
- **B. Type the command without Enter** into a new terminal tab in the file's directory; you press Enter.
- **C. Confirm first.** A popup shows the command, then it runs.

**Recommendation:** (a) C and (b) B. Nothing leaves the machine without a command, and nothing runs without your Enter.

**Your answer:**
>

---

## 17. Markdown language server and formatter

**Source:** docs/TODO.md entry "DISCUSSION: Markdown language support (LSP, formatting)" (line ~1008)

**What's being decided:** whether Markdown language features come from an external server, from our own first-party server, or both.

**Background:**
- `crates/lsp` supports both in-process first-party servers (like the config server) and spawned ones. The config section that maps file types to external servers isn't built yet ("Language services — follow-ups" item 2).
- Marksman is an MIT-licensed standalone binary covering links, references, heading rename, and broken-link diagnostics. It doesn't format.
- External formatters exist (dprint's Markdown plugin, prettier, mdformat), and each brings its own runtime.

**Options:**
- **A. External.** Marksman through the server mapping, plus a formatter command in config.
  - Pros: little code.
  - Cons: you install things; its behaviour may not match how our renderer reads Markdown.
- **B. In-tree first-party server.**
  - Pros: document symbols (feeding the sticky table of contents), link diagnostics (item 16), and formatting rules you choose.
  - Cons: build effort.
- **C. In-tree for symbols and diagnostics; no formatter** until you name the rules you want.

**Recommendation:** C. It reuses our Markdown highlighter's parse and feeds features already planned.

**Your answer:**
>

---

## 18. Indexer scope

**Source:** docs/TODO.md entry "Indexer: Markdown free text and/or code symbols" (line ~1280); "Double-tap rework + fuzzy file-search popup" (line ~545: symbols later)

**What's being decided:** what to index. You said: "I'm not sure if that would be like a free form text indexer for Markdown or if it would be code only for symbols."

**Background:**
- No index exists. The recursive fuzzy file walk stops at 20,000 entries (`FILTER_WALK_CAP`).
- Search in files is being built on `feat/files-6-search` (issue #1).
- Symbols could come from LSP `workspace/symbol` (needs servers running) or from tree-sitter tag queries (if item 6 goes with A).
- A persisted free-text index means a trigram index of our own, or a large dependency such as `tantivy`.

**Options:**
- **A. No persistent index.** Parallel on-demand scans, plus LSP workspace symbols where a server runs.
- **B. An in-memory symbol index** per workspace root, built from tree-sitter tags and updated on file change.
- **C. Free text and symbols, persisted** in the state dir.

**Recommendation:** A now; B once item 6 is decided; C only if scans get slow on your repos. Also tell us what you want to find that on-demand search wouldn't give you.

**Your answer:**
>

---

## 19. Run Anything inside the palette

**Source:** docs/TODO.md entry "Double-tap rework + fuzzy file-search popup" (line ~535: "fold the one-shot 'Run Anything' terminal into it somehow"); "Double-tap triggers: make them configurable" (line ~89)

**What's being decided:** how the one-shot shell command runner fits into the command palette once double-Ctrl opens the palette and double-Shift opens file search.

**Options:**
- **A. A prefix.** Starting the query with `$ ` switches the palette to run mode (VS Code's quick open uses `>` the same way).
  - Pros: explicit; Enter never runs a shell command by accident.
- **B. A fallback row.** A "Run `<text>` in a terminal" row is always last.
  - Pros: no prefix to learn.
  - Cons: when no command matches, that row is the only one, and Enter runs your typo in a shell.
- **C. Mode tabs.** Commands / Run / Files tabs in the palette header, cycled with Tab.

**Recommendation:** A.

**Your answer:**
>

---

## 20. Shortcuts sheet

**Source:** docs/TODO.md entry "Shortcuts sheet for the focused pane (title-bar button)" (line ~640: "idea, not committed to"; open: where the button lives, and whether the text keymap becomes configurable)

**What's being decided:** whether to build the sheet, how you open it, and whether the text-editing keymap (`plugin::text_edit::Keymap`, fixed per platform) becomes configurable.

**Background:**
- The terminal's readline translation (primary screen only) reads the same keymap, so making the keymap configurable changes what the terminal sends too.
- The sheet needs views to declare their own chords, which is plugin follow-up (4).

**Options:**
- **A. A read-only overlay** opened by a command and chord (`help.shortcuts`); no title-bar button. The text keymap stays fixed.
- **B. A title-bar button** beside the sidebar toggle, as in the TODO. This needs space in the title bar, which macOS's full-size content view doesn't give by default.
- **C. Like A, plus an editable text keymap** in `[keybindings.text]`.
- **D. Defer.**

**Recommendation:** A, read-only. Revisit C later.

**Your answer:**
>

---

## 21. Web preload / prerender default

**Source:** docs/TODO.md entry "Web — preload on link hover and URL-bar typing" (line ~720)

**What's being decided:** the default for `[web] preload = off | connect | prerender`.

**Background:**
- Prerendering issues real GET requests. That can trigger side effects, counts as an analytics hit, and tells the site what you were about to open.
- `connect` only opens a connection early.

**Options:**
- **A. `off`.**
- **B. `connect`** for human profiles; `off` for agent profiles (agents don't hover).
- **C. `prerender`** for human profiles.

**Recommendation:** B.

**Your answer:**
>

---

## 22. Comment mode storage

**Source:** docs/TODO.md entry "Comment mode" (line ~1341); GitHub issue #1

**What's being decided:** where comment threads anchored to lines live, and who can see them. The two uses are talking to agents about specific lines and showing a PR's review threads.

**Options:**
- **A. Local.** A sidecar in the state dir, keyed by path plus a content anchor so a comment survives edits.
  - Pros: private; never committed.
- **B. In the repo** (for example `.ghostrealm/comments/`).
  - Pros: shared through git.
  - Cons: noise in diffs; committed by accident.
- **C. GitHub only.** Read and write PR review comments through the API or `gh`.
- **D. A for your own and agent comments, plus C to show PR threads.**

Agents read comments through the agent channel, within item 3's scope.

**Recommendation:** D. Nothing is written into the repo unless you ask.

**Your answer:**
>

---

## 23. GIT.md "committing is pre-authorised"

**Source:** docs/stack-review-findings.md "To confirm" (line ~21); docs/GIT.md first cardinal rule

**What's being decided:** whether the rule "COMMITTING IS PRE-AUTHORISED", which replaced "do not commit without explicit authorisation" in `41d9d6b`, is intended.

**Background:**
- The rule lets agents commit on feature branches without asking.
- What stays unchanged: commits to `develop`, `main`, or `master` are still forbidden, and pushes still need per-request authorisation.

**Options:**
- **A. Confirm it.** Branch commits are free, and review happens before `develop` moves.
- **B. Revert to per-commit approval.**

**Recommendation:** A. It matches how work is reviewed now, and the push rules still guard the shared branches.

**Your answer:**
>

---

## Decisions I'd take by default unless you object

Each line gives the item and the default that will be applied if you don't veto it.

**Web**
- **Pages an agent drives stay visible to Chromium** (`docs/DECISION.md`, Web pop-ups; built in 8af2376): agent-owned pages, including hidden pop-ups and pages created off screen, report themselves visible to Chromium (as headless Chrome and Puppeteer do), at 10 fps while nobody sees them; a person's background tabs stay hidden. Alternatives: show a page only while an eval runs (pages see `visibilitychange` around each eval), or wait longer for hidden pages (blocks the UI thread up to 30 s).

**Terminal**
- **Closing a pane stops its program** (`crates/terminal-ghostty/src/threaded.rs`, built in dbefd79): closing a pane hangs up its terminal; a program still running 2 s after the close gets SIGHUP (its process group and the foreground group), then SIGKILL 1 s later; nothing is signalled once the shell has been reaped, and the reaper gives up after 30 s. A program that needs longer to clean up on SIGHUP (an editor writing a recovery file, a dev server shutting down) is cut off at 3 s. Alternatives: longer graces (e.g. SIGKILL at 10 s), or no SIGKILL at all (a `trap '' HUP` program outlives its pane).

**Toasts over modals**
- **Toast choices while a modal is open** (`docs/DECISION.md`, the error surface's toast detail): while the picker or a confirmation is open, a toast's choices (e.g. "Save As…", "Open Settings") are inert and Dismiss stays live; error and warning toasts stay until dismissed, so the choice is there once the modal closes. The alternative is for a choice to cancel the picker or confirmation first, discarding what was in progress.

**Disk**
- **Automatic cleanup hook** (`.claude/skills/build/SKILL.md`, clean-all): no `SessionStart`/`SessionEnd` hook in `.claude/settings.json`; the director runs `mise run clean-all` after each goal lands. A hook would be lock-aware, but it can't pass `--except` for agent worktrees awaiting review, so it would empty their `target/` once idle 2 h and the reviewer would rebuild.

**Terminal**
- **Unbracketed paste** (findings-2 item 36): sanitise like xterm and Ghostty: strip control bytes and send newlines as CR. Flip the backend test `paste_is_unwrapped_when_the_program_did_not_ask_for_brackets`.
- **Mouse** (TODO line ~60): alternate scroll (mode 1007) on, as in Ghostty; the horizontal wheel reported as buttons 6/7.
- **Terminal soft-wrap toggle** (TODO line ~329): skip it.
- **Wider-than-pane terminal grid** (TODO line ~354): defer the spike.

**Workspaces, panes, tabs**
- **Close workspace confirmation** (TODO line ~220): confirm when the workspace has more than one pane. Unsaved documents already confirm.
- **htab context menu** (TODO line ~218): Rename, Close, Close Others.
- **htab overflow and reorder** (TODO line ~242): scroll horizontally on overflow; drag to reorder.
- **Palette labels** (TODO line ~166): no scope prefix; the label names the object ("New Workspace", "New Tab").
- **Double-tap remapping** (TODO line ~89): `[input] double_tap_shift = "<command id>"` and `double_tap_ctrl = "<command id>"`.

**Config**
- **Keybinding ids without quotes** (TODO line ~1018): flatten nested tables under `[keybindings]` as proposed, and report an id that is both a command and a prefix of another id as a config finding.
- **Colour values** (TODO line ~316): accept `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb()`, and `rgba()`.
- **Header ribbon** (TODO line ~1331): `[editor] ribbon = ["soft_wrap", "autosave"]`.
- **External LSP servers** (TODO line ~616): `[lsp.<language>] command = [...]`; no servers bundled.

**Files**
- **Stuck network mount** (findings-2 item 24): no change. The cited `preview.rs` is gone, and each Files document now loads on its own thread (`plugins/files/media.rs`), so a dead mount stalls only that document's "loading" state.
- **Section outline** (TODO line ~564): issue #1's sticky table of contents, nested by heading level, marking the section at the top of the view.
- **File menu edges** (TODO line ~906):
  - New File opens the file in a kept tab.
  - Cmd+Backspace moves the selection to the Trash, with the same confirmation as the menu.
  - Copy and Cut also put file URLs on the system pasteboard, and Paste accepts Finder's.
- **Linux Trash** (TODO line ~917): implement the freedesktop Trash spec in-house when Linux work starts, or use the `trash` crate, per your item 7 answer.
- **File browser polish** (TODO line ~408):
  - The date filter uses modified time.
  - An `ext:rs` token in the filter box sets the extension filter.
  - Gitignored entries in a greyed red, ignored folders and directories bold.
- **Scroll-to-top/bottom buttons** (TODO line ~1293): shown bottom-right once you are more than a screen from that end. In a terminal, "bottom" returns to live output.

**Editor and Markdown**
- **Image alpha blending** (findings-2 item 43): keep linear-light blending. Switch only if PNG edges look wrong next to a browser on your display.
- **Markdown link colours** (TODO line ~462): internal links in the accent colour, underlined; external links in a second accent; broken links with a red wavy underline.

**Web**
- **Web host lifetime** (TODO lines ~664, ~748):
  - Exit 30 s after the last web surface closes.
  - Restart automatically after a crash, with a note in the affected panes.
  - Devtools as a `web.devtools` command.
- **CEF updates** (TODO line ~1306): a local command, behind the same entry point item 2 picks, that builds and tests the new CEF and removes the old one only after that passes. CI later; the repo has no CI yet.

**Performance**
- **"Proper renderer" epic** (TODO line ~284): defer until a `--release` profile on a real display shows UI-thread shaping or `prepare` as the bottleneck.

## Needs you at a desktop (not decisions)

These can't be verified in the headless sandbox:
- **Feel of the perf work:** "Verify the perf work on a real display" (line ~75), "Scroll lag" (line ~133), and "Responsiveness — heavy output" (line ~147). Check the feel on a `--release` build.
- **Web pages:** "Web surface — follow-ups" (1) (line ~665): page drawing, HiDPI, cursors, dead keys, and clipboard in pages.
- **GPU compositing:** "Web — GPU (accelerated) off-screen rendering" (line ~736): does GPU compositing paint on a real display?
- **Nested `.gitignore`:** "File browser — remaining polish" (6) (line ~420): confirm it on a real repo.
- **Error surface:** toasts (colours, placement at the top of the pane or below the top tab strips for app-wide ones, hover highlight and dismiss on mouse-up, auto-hide timing), the Save As picker's inline error and red border, the palette prompt's error rows and their tooltip, wrapped tooltips, and the wavy red/yellow underlines in the editor. Also the palette's info toast for a command that doesn't apply (e.g. Toggle Comment in a file with no comment syntax), the error toast when a Save As picker's document has closed, and a web pane's toasts for a failed Back, Forward, Reload or address (one per web pane, each replacing the last). Two TODO entries also need a display: a launch-time toast under a still pointer, and wide glyphs in prompt error rows.
- **Heading spacing and sizes:** open a Markdown file in Integrated and in Preview. Check the larger headings and the space above them (defaults in `[headings]`: `h1_scale`..`h6_scale`, `h1_space`..`h6_space`; hover the section in `config.toml` for docs), a click in the space above a heading, a table-of-contents jump, trackpad scrolling past headings, and live tuning by editing the values.
- **Stacked modals and toasts:** Save As onto an existing file: the picker stays visible and dimmed under the Replace? question, with no picker text showing through. A toast raised while the palette, picker or a confirmation is open draws on top, its × works, its choices are inert under the picker or a confirmation and work once it closes, and hover, click and scroll don't reach the modal beneath; an info toast over a modal hides after about 4 s. A context menu and tooltips draw over the toast. Editor hover popups and images still draw, with no text bleeding between layers. Judge whether a toast covering the palette's input line is acceptable.
- **Full-screen program keys:** on branch `fix/alt-screen-native-keys`, check word-left, Shift+Left and Option+Up in vim/nvim, less and htop in the app.
- **CEF host tests:** `ghostrealm-web --test host` (10–12 tests) and `popups_with_the_real_host` fail on `develop` in agent runs. See the TODO entry; check whether they pass on your machine with a fresh bundle.

## Parked (no decision needed yet)

- Shell LSP assist (IDEAS.md line ~26, "exploratory — not yet").
- Out-of-tree or WASM third-party plugins ("Plugin architecture — follow-ups" (2) and (6), "if ever").
- A Kotlin Preview mode with rendered doc comments and diagrams (issue #1).
- GitHub permalinks (TODO line ~1226, "future idea; not now").

## Stale TODO entries

These are already done or no longer apply (checked against `develop` at `311033e`):

- **"Text-editor pane — follow-ups" (line ~113).** Every item listed is done:
  - the unsaved indicator (the dot)
  - `editor.save_as`
  - click-to-position and drag-select (`MouseEvent::Drag` in `plugins/editor/mod.rs`)
  - word motion (`plugin::text_edit`)
  - horizontal scroll (`EditorBuffer::hscroll`)

  Only the pointer to the shell-LSP idea remains.
- **"ARCHITECTURE — VT off the UI thread — DONE (92d5ca0)" (line ~262).** Done. Its verification step duplicates "Verify the perf work on a real display".
- **"Taller workspace tabs showing the pinned directory" (line ~275).** Done: a named workspace draws its name over its greyed directory on two lines (`window.rs`, the sidebar list).
- **"Naming — rename vertical tabs → workspaces" (line ~160).** The UI rename is done (New/Close/Rename Workspace; GLOSSARY.md uses Workspace). Only the internal `Vtab` → workspace rename is left.
- **Mouse entry (line ~60).** "the plugin API delivers only the left button" no longer holds: right and middle clicks are reported (`report_button`, `plugins/terminal.rs` line ~667). Still open: the horizontal wheel and mode 1007.
- **"Splits — discoverability" (line ~227), item (1).** Default split chords exist in the new-config text (`cmd+d`, `cmd+shift+d` in `default_bindings`, `core/src/config.rs`). Item (2), the pane buttons, is open.
- **"Plugin architecture — follow-ups" (line ~501), item (5).** The web-browser plugin is done (`plugins/web`, `PaintCx::image`).
- **"Markdown rich text — follow-ups" (line ~441).**
  - "Headings differ only in weight" is done (`ec8bdd3` and the heading fixes after it).
  - "The file browser's preview colours Markdown but does not draw the styles" no longer applies: the file browser has no preview beside its list (DECISION.md, 2026-10-17).
- **Findings-2 item 34, "The preview follows hover"** (also listed in TODO "Decision session"). No longer applies, for the same reason.
- **"File browser — alternate views (grouped)" (line ~366).** Mostly done: the Extension, Kind, Git status, Modified, and Size views (`FsView`, `core/src/fs_tree.rs`) and the dim disambiguating prefix exist. Left: the flat "recent" and "largest files" lists.
- **"Decision session: open 'Decision needed' items" (line ~820).** This file replaces it. Its "editor and file browser as one surface" item is decided (issue #1).
- **IDEAS.md "Built-in web browser pane" (line ~35).** Done (`plugins/web`, `crates/web-host`).
