# Stack review findings (2)

Open bugs found reviewing `1e1ff67..8a77896`. Line numbers are against
`docs/stack-review-findings` (`9416eac`). "Confirmed" means a throwaway test
reproduced it; the rest come from reading the code. Items marked "Decision
needed" wait on a product call.

## Low

7. **Terminal readline translation applies inside full-screen apps.** Word/line
   motions become readline keys ([`plugins/terminal.rs:594`](../crates/app/src/plugins/terminal.rs),
   from `903b54d`) in the alternate screen too, and on Linux/Windows they replace
   the Ctrl+arrow/Home/End CSI sequences. Decision needed: translate only
   outside the alternate screen / application-cursor mode, or always (as iTerm's
   "natural text editing" does)?
13. **Hover popups paint over modals.** The editor's hover popup
    ([`plugins/editor/lang.rs:257`](../crates/app/src/plugins/editor/lang.rs),
    painted from [`plugins/editor/mod.rs:653`](../crates/app/src/plugins/editor/mod.rs),
    from `7654ce1`) draws its text over the Save As/Open picker and the confirm
    dialog; it isn't dismissed when a modal opens, its dwell timer can open it
    under one, and with the palette or a menu open its box draws under the dim.
    Fix: dismiss view popups when a modal opens and skip view overlays while one
    is open.
14. **Missing `[keybindings]` binds nothing, silently** (`019dc17`). Files
    written before then carry the comment "If the whole section is empty, the
    built-in defaults apply", which is now false. Decision needed: a missing
    section means the defaults, or stays strict with a warning finding?
15. **Load-time findings go only to stderr** (`load_or_create`,
    [`config.rs:469`](../crates/core/src/config.rs), `019dc17`). New clamps
    (`sidebar.width` min 40, `font_size` max 96, `double_tap_window_ms` max 2000)
    change existing files' behaviour silently when launched from Finder.
    Decision needed: surface them how (inbox, toast, settings status)?
16. **Config check nits** ([`core/src/config/check.rs`](../crates/core/src/config/check.rs)):
    a duplicate chord (`:145`) is a Warning though the binding is dropped (an
    Error by the severity definitions); `nan`/`inf` for a number reads "expects
    number 40–1000, found float" (`:197`).
17. **Path/URI comparisons are exact (latent).** The config language service
    matches `config_path == path` ([`languages.rs:27`](../crates/app/src/languages.rs)),
    so a symlinked path misses; `file_uri` ([`lsp/src/text.rs:105`](../crates/lsp/src/text.rs))
    percent-encodes RFC 3986 sub-delimiters and is lossy for non-UTF-8 paths;
    the client compares diagnostics URIs as strings
    ([`lsp/src/client.rs:252`](../crates/lsp/src/client.rs)), dropping them from
    servers that normalise URIs. Fix: canonicalise paths; encode per RFC 3986's
    path grammar; compare decoded paths.

## Web browser and file-browser preview

Review in progress.
