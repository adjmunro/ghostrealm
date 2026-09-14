# Goal.md

Non-technical goals and shape of the product. Technical detail lives in
[SPEC.md](SPEC.md); decisions and their reasons in [DECISION.md](DECISION.md).

## What we're building

A native desktop **terminal multiplexer** that hosts real terminal surfaces
inside our own window chrome. The differentiation is everything *around* the
terminal, not the terminal itself:

- A **vertical tab sidebar** that behaves like an email inbox: tabs are
  `read` / `unread` / `busy`, with a sticky `needs_input`. Background jobs that
  finish while you're elsewhere surface as unread; interactive use doesn't spam.
- **Splitting** in both directions, recursively, each split panel with its own
  **horizontal tab strip**.
- A **command palette** (JetBrains "Search Everywhere" style) over a single
  command registry.
- An **agent control channel**: an agent (e.g. Claude) running in a tab can both
  drive the app (search/run commands) and report its own status/label back to it,
  scoped to its own vertical tab plus app-wide settings.

## Platforms

macOS and Linux. **Ship macOS first.** Windows is out (libghostty has no Windows
support). Cross-platform is a real goal, which is why we build on the VT engine
and own the renderer rather than the macOS-only embedder — see DECISION.md.

## Configuration

A single shareable TOML dotfile at an XDG path, designed to be committed/synced.
Terminal styling inherits from Ghostty's own config; chrome styling inherits the
resolved palette unless overridden.
