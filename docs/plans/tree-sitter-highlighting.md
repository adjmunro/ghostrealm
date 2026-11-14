# Plan: syntax highlighting with tree-sitter

`decisions-pending.md` item 6, answered A: code languages are highlighted by
tree-sitter (the `tree-sitter` crate plus per-language grammar crates);
Markdown and TOML keep their hand-written highlighters; the `Token`
vocabulary is split first. Item 7 puts the dependency behind our own
interface. The need: code in the editor is coloured accurately, re-coloured
incrementally as it is edited, tolerant of broken code, on a syntax tree that
the outline, Kotlin heading modes, fence highlighting and the indexer can
later share.

## Settled by the code today

- `ghostrealm_core::highlight` holds two line-local `Highlighter`s, TOML and
  Markdown (`ALL`), found by extension (`for_path`). A highlighter maps one
  line plus a `u32` `State` to `Span`s of `Token`s. `StateCache` and
  `SpanCache` keep paint cheap. A syntax tree does not fit this trait: it is
  per document, not per line, and its state is a tree, not a `u32`.
- `highlight::LANGUAGES` lists comment syntax for languages without a
  highlighter (Kotlin, Java, Rust, Swift among them). The comment toggle and
  file icons read it through `language_for_path`.
- `Token` consumers: `app/src/syntax.rs` (`color`, `style`, `build`), the
  Markdown and TOML highlighters, `md_preview` (code colour), editor tests.
- Every text change in `EditorBuffer` passes through one of three places:
  `splice` (typing, deletes, paste, comment toggle, reload), `sync` applying
  the history's `Op`s (undo, redo, `goto`, other views' edits), and `sync`
  taking the text whole when its step was pruned. An `Op` is `{ at: (row,
  char col), del, ins }`.
- `outline.rs` already runs long work on a worker: an `mpsc` job, a `Waker`
  to repaint, one job at a time, a test gate. Tree-sitter parsing reuses that
  shape.

## Step 1: split `Token`

Now: one flat enum mixing code categories (`Comment`, `Key`, `String`,
`Number`) with Markdown rich-text ones (`Heading`, `Marker`, `Strong`,
`Emphasis`, `Code`, `Link`, `Url`, `Quote`). Code languages need categories
it lacks (keyword, type, function, ...).

Becomes:

```rust
pub enum Token { Plain, Syntax(Syntax), Rich(Rich) }
/// What code is; a colour only.
pub enum Syntax { Comment, Keyword, String, Number, Constant, Type, Function,
                  Property, Attribute, Operator, Punctuation }
/// Rich-text structure; a colour and a style.
pub enum Rich { Heading, Marker, Strong, Emphasis, Code, Link, Url, Quote }
```

- `Key` becomes `Syntax::Property` (TOML keys, a Markdown fence's language
  word). TOML table headers stay `Rich::Heading`: Integrated draws them
  bold, as it does now. Markdown's HTML comments stay
  `Syntax::Comment`, its link titles `Syntax::String`.
- One token per char stays, so `runs`, `Span`, both caches and every
  highlighter signature are unchanged.
- `syntax::color` matches both families; existing tokens keep their RGB, so
  TOML and Markdown look the same. New colours for `Keyword`, `Type`,
  `Function`, `Constant`, `Attribute`, `Operator`, `Punctuation`, in the
  same muted range, settled at the desktop check.
- `syntax::style` matches `Rich` only: a code language can never come out
  bold, boxed or underlined.
- Consumers changed: `core/highlight/{mod,markdown,toml}.rs`,
  `app/src/syntax.rs`, `md_preview` (one call), two editor tests, the
  token list in PLUGINS.md "Syntax highlighting".

Behaviour-preserving; it lands on its own.

## Step 2: tree-sitter behind our interface

### The seam

Like `terminal` / `terminal-ghostty`: the interface lives in core, the
implementation in its own crate, and only the app names both.

- `ghostrealm_core::syntax` (new, no tree-sitter types):
  - `TextEdit { start_byte, old_end_byte, new_end_byte, start, old_end,
    new_end }`, points as (row, byte col), and `TextEdit::of(lines, at,
    del, ins)` building one from an `Op`-shaped change on the text after it
    (the prefix before `at` is the same before and after).
  - `TextEdits`: the log a buffer keeps, `{ reset: bool, edits:
    Vec<TextEdit> }`; `reset` means "re-parse from scratch".
  - `trait SyntaxDoc`: one document's tree, kept in step with its text.
    `name()`, `update(&mut self, edits: TextEdits, lines: &[String],
    count: u64)` (apply the edits, reparse here or on the worker; returns
    whether the spans changed), `highlight(&mut self, lines, rows:
    Range<usize>)` then `line(&self, ln) -> &[Span]`, `working()`.
- `crates/syntax-tree-sitter` (package `ghostrealm-syntax-tree-sitter`):
  depends on core, `tree-sitter`, `streaming-iterator`, the grammar crates.
  Exposes `open(path, waker) -> Option<Box<dyn SyntaxDoc>>` and the language
  registry. Nothing else in the workspace names a `tree_sitter` type.

A separate crate keeps the C builds out of `ghostrealm-core`'s own test
builds and makes a swap a change to one crate. The alternative, a module in
core, saves a 20-line manifest.

### Languages in this increment

| Language | Extensions | Grammar crate | Why now |
| --- | --- | --- | --- |
| Kotlin, Gradle KTS | `kt`, `kts` | `tree-sitter-kotlin-ng` 1.1.0 | The owner's main language; `build.gradle.kts` is Kotlin script and parses with the same grammar (checked: a Gradle KTS sample parses without an ERROR node). |
| Java | `java` | `tree-sitter-java` 0.23.5 | First tier with Kotlin. |
| Rust | `rs` | `tree-sitter-rust` 0.24.2 | Second tier, and the language of this repo, which the owner reads in the editor. Its official grammar and query are the most mature, which makes it the reference for the mapping tests. |

Next increments, one commit each (a dependency, a registry line, a mapping
check): Swift (`tree-sitter-swift` 0.7.4: its query compiles on 0.27.1 with
40 captures), then web languages. Every other entry in `LANGUAGES` stays
plain. Each listed language keeps its `LANGUAGES` entry: the comment toggle
and Markdown fences look comment syntax up there, and moving it is the
fence-highlighting step's job.

**Kotlin grammar.** Two candidates, both MIT:

- `tree-sitter-kotlin-ng` 1.1.0 (crates.io, tree-sitter-grammars org,
  1.1 MB crate). Last release 2025-01; ships no highlight query.
- `fwcd/tree-sitter-kotlin` (what nvim-treesitter and Helix use; active,
  last commit 2026-10-31). Its crates.io release (0.3.8) needs
  `tree-sitter < 0.23`, so it would be a git dependency pinned by rev,
  whose first fetch clones 177 MB (178 s here). Its shipped
  `highlights.scm` does not compile against its own grammar
  (`QueryError: NodeType "simple_identifier"`).

Both parse a 70-line Kotlin sample (sealed/value/data classes, generics with
`where`, `when`, raw strings, labels, `0..<n`) and a Gradle KTS file with
zero ERROR nodes. Either way we write the Kotlin query. Recommended:
`kotlin-ng`; it adds nothing beyond the goal. Switching later replaces one
dependency and the query.

### Highlight queries and the mapping

- Java and Rust use the `HIGHLIGHTS_QUERY` their crates ship (MIT). Kotlin
  uses `crates/syntax-tree-sitter/queries/kotlin.scm`, ours, written
  against `kotlin-ng`'s `node-types.json` (about 100 lines).
- The text predicates these queries use (`#match?`, `#eq?`, `#any-of?`)
  are evaluated by the `tree-sitter` crate.
- A capture name maps to a `Syntax` by its longest listed prefix (dropping
  dotted segments from the right), once per query at load:

  | Captures | `Syntax` |
  | --- | --- |
  | `comment*` | Comment |
  | `keyword*`, `include`, `conditional`, `repeat`, `exception`, `variable.builtin` | Keyword |
  | `string*`, `character*`, `escape` | String |
  | `number*`, `float` | Number |
  | `constant*`, `boolean` | Constant |
  | `type*`, `constructor`, `namespace`, `module` | Type |
  | `function*`, `method*` | Function |
  | `property`, `field`, `variable.member` | Property |
  | `attribute`, `annotation`, `decorator` | Attribute |
  | `operator` | Operator |
  | `punctuation*` | Punctuation |
  | `variable`, `variable.parameter`, `label`, `_`-prefixed, anything else | none (plain) |

- Precedence: captures on the same node keep the first pattern's (the
  upstream query convention); a node nested in another overrides it for its
  own range (an escape inside a string).
- Injections (Rust doc comments, macros) and locals are left out.

### Incremental reparse

- `EditorBuffer` gets `pub text_edits: Option<TextEdits>`, `None` until a
  view holding a `SyntaxDoc` turns it on, so other files pay nothing.
  `splice` and `sync`'s route push `TextEdit::of(..)` for each change;
  `sync`'s whole-text path sets `reset`. Over 256 queued edits the log
  collapses to `reset` (a 1,000-line comment toggle reparses once).
  `TextEdit::of` sums the line lengths before the edit: about 0.5 ms at a
  million lines, nothing on ordinary files.
- The view takes the log where it takes `dirty_from` today, before a paint,
  and hands it to `SyntaxDoc::update`, which calls `Tree::edit` for each
  edit in order, then reparses with the edited tree as the old one.
- A reparse on the UI thread reads the buffer's lines in place through
  tree-sitter's input callback (a row's bytes from the point's column, then
  `\n`), with no copy.

### Threading and latency

Measured on this machine (tree-sitter 0.27.1, `tree-sitter-rust`), the
repo's Rust sources concatenated: 82,651 lines, 3.36 MB.

| | release | dev |
| --- | --- | --- |
| Full parse | 491 ms | 1.99 s |
| Reparse after typing one char mid-file | 7.8 ms | 35 ms |
| Reparse after typing `/*` mid-file | 20 ms | 60 ms |
| Query over 80 visible lines | 0.25 ms | 1.1 ms |
| Compile the Rust highlight query (once per language) | 21 ms | 101 ms |

The incremental reparse equals a full one (same S-expression), and its
changed range is the one edited line.

- `WORKER_BYTES` = 64 KiB (about 9 ms of full parse, release). Below it
  every parse, first and incremental, runs on the UI thread. From it every
  parse runs on a worker: a clone of the tree (cheap, `Tree` is `Send`) and
  one `join("\n")` copy of the text (about 11 ms per million lines, as the
  outline worker pays), one job at a time, a waker to repaint.
- While a job runs, edits still go through `Tree::edit` on the UI thread's
  tree and are queued. When the job's tree lands, the queued edits are
  replayed onto it; if the text moved on, the next job starts from it. A
  `reset` drops the job.
- Meanwhile paint queries the edited old tree: unchanged text keeps its
  colours, shifted to where it moved; the edited stretch may be coloured
  wrong until the job lands. Before a large file's first tree lands it
  shows plain.
- Spans of a range are cached per row and dropped when the tree or the
  text changes, so a paint without edits runs no query.
- The query is compiled the first time a file in that language opens, on
  the UI thread (21 ms once, release).

### Unknown languages and failures

- No grammar for the extension: no `SyntaxDoc`. The file keeps the line
  highlighter if it has one (TOML, Markdown), else stays plain. Comment
  toggle and icons are unchanged (`LANGUAGES`).
- A query that fails to compile: that language opens plain. A test compiles
  every shipped query, so this only follows a grammar update.
- Broken code: tree-sitter returns a tree with ERROR and MISSING nodes;
  matched nodes are coloured, the rest plain. Spans always cover the line.

### View modes

A tree-sitter language offers Source only: Integrated would differ from
Source only by heading sizes and code boxes, which code has none of until
the Kotlin heading modes. `modes_for` is unchanged (`highlighter()` is
`None` for these files). DECISION.md's view-mode detail says "other
highlighted languages Integrated and Source"; it becomes "TOML Integrated
and Source".

## Build impact

- Versions (today 2026-11-15; every one at least two weeks old):
  `tree-sitter` 0.27.1 (2026-10-08; 0.27.2 is 8 days old),
  `tree-sitter-language` 0.1.9 (2026-10-08), `streaming-iterator` 0.1.9,
  `tree-sitter-kotlin-ng` 1.1.0 (2025-01-09), `tree-sitter-java` 0.23.5
  (2024-12-21), `tree-sitter-rust` 0.24.2 (2026-03-27).
- New crates in the tree: those six. `cc`, `regex` and `serde_json` (build
  only) resolve to the versions already locked. `tree-sitter`'s optional
  `wasm` feature (wasmtime) stays off.
- `tree-sitter` 0.27 declares rust-version 1.90; the workspace says 1.88,
  but `libghostty-vt` and `ordered-float` already need 1.90, so nothing new.
- Licences: all MIT (`streaming-iterator` MIT OR Apache-2.0). No GPL, no
  LGPL. Our Kotlin query is ours.
- Build time, clean, 8 cores, measured in a scratch crate with Swift
  included: each grammar's build script takes 4.3 to 6.5 s (C via `cc`), run
  in parallel; about 10 s wall for tree-sitter plus grammars in dev, 16 s in
  release.
  In the workspace they overlap the Ghostty (Zig) build. Rebuilds don't
  rerun them. Binary: `libtree-sitter-kotlin.a` 3.5 MB, rust 1.1 MB, java
  0.4 MB, runtime 0.3 MB (release).
- Toolchain: grammar C compiles with the system C compiler through `cc`, not
  Zig: nothing for `mise.toml` or the build skill. Linux needs a C compiler
  on `PATH` (`cc`); SPEC.md's stack notes say so. No `cfg` code.

## Out of scope, and the room left for each

- **Outline from the tree, Kotlin heading modes**: `SyntaxDoc` gains a method
  over the same tree (from a `tags.scm`-style query per language), feeding
  `Outline` as a second source beside the line highlighter's. The registry
  entry is where a language's further queries go.
- **Fence highlighting**: the registry is keyed by names as well as
  extensions (`["kotlin", "kt", "kts"]`), so a fence's info word finds a
  grammar; a one-shot `highlight_text(name, lines)` over the same mapping
  serves Preview and Integrated code blocks. Markdown's `u32` state can't
  carry a tree, so this lives at the view, not in the Markdown highlighter.
- **Indexer**: `Tree` is `Send` and the registry is static, so a worker can
  parse files without a view.
- Also out: injections, locals, TOML/Markdown on tree-sitter, theme
  configuration of syntax colours, LSP semantic tokens, Swift and web
  grammars (next increments), Integrated mode for code.

## Tests

Core (`syntax`, `highlight`):

- Token split: `color` of every existing token is unchanged (TOML and
  Markdown row tests keep their RGB); `style` is plain for every `Syntax`.
- `TextEdit::of`: on single-line, multi-line, multi-byte (`é`, `🦀`) and
  CRLF text, splicing the old text's `start_byte..old_end_byte` with the new
  text's `start_byte..new_end_byte` gives the new text, and the points
  match the byte offsets.

Buffer (`editor/buffer.rs`):

- With the log on, typing a word, a Backspace run, typing over a selection,
  an auto-closed bracket, a multi-line paste, undo, redo, a jump across
  branches, another view's edit synced in and a reload each log edits that
  replay (as above) from the old text to the new. A pruned step logs
  `reset`; 300 edits collapse to `reset`; with the log off nothing is
  recorded.

Tree-sitter crate:

- Every language loads (ABI accepted) and its query compiles; every capture
  name its query uses is in the mapping table or listed as plain, so a
  grammar update with new names fails here.
- Mapping: capture names to `Syntax` per the table, including dotted
  fallback, `variable.builtin`, `_`-prefixed and unknown names.
- Token mapping per language: a snippet with expected tokens for named
  words. Kotlin: `fun`, `val`, `when` Keyword; `String` Type; `"a$b"`
  String; `// c` Comment; `@JvmStatic` Attribute; `42` Number; `println(`
  Function; plus a Gradle KTS snippet. Java and Rust likewise.
- Precedence: an escape inside a string; a node captured by two patterns
  takes the first's token.
- Incremental equals full: for each language, a seeded sequence of edits
  (inserts and deletes within and across lines, multi-byte chars, a block
  comment opened and closed) through `update`; after each, the tree's
  S-expression and every line's spans equal a fresh parse's.
- Error tolerance: a Kotlin file with an unclosed string, a missing `)` and
  an unclosed brace colours keywords after the break; spans cover every
  char of every line; random byte-level edits never panic.
- A block comment opened above the queried rows colours the rows inside it.
- Large file (worker): a document over `WORKER_BYTES` with the test gate
  held: `update` returns with `working()` and plain spans; released, the
  tree lands through the waker and spans equal a UI-thread parse's. Edits
  made while the job runs are replayed and a second job brings the tree to
  the latest text; a `reset` meanwhile drops the job's result.

Editor (`plugins/editor`, through `Harness`):

- A `.kt` view paints its rows in the mapped colours (`text_item` keys, as
  the Markdown fence test does); `.kts`, `.java` and `.rs` each colour one
  keyword.
- Typing `fun main() {` (with its auto-closed brace), a Backspace run,
  typing over a selection, then undo and redo: the painted rows equal those
  of a fresh view opened on the same text.
- A `.kt` file offers Source only; its comment toggle still uses `//`; a
  `.toml` and a `.md` file paint as before; a file with an unknown
  extension paints plain.

## Commits and size

Insertions plus deletions, excluding `Cargo.lock`:

| Commit | Lines |
| --- | --- |
| 1. `refactor(highlight): split Token into Syntax and Rich` | ~240 |
| 2. `feat(syntax): text edit log and the SyntaxDoc interface` (core seam, buffer log, tests) | ~250 |
| 3. `feat(syntax): tree-sitter documents for Kotlin, Java and Rust` (crate, registry, Kotlin query, worker, tests) | ~700 |
| 4. `feat(editor): highlight code languages with tree-sitter` (wiring, editor tests, PLUGINS.md, SPEC.md crate list, DECISION.md view modes, TODO.md) | ~200 |
| **Total** | **~1,400** |

Commit 1 can land on its own, reviewed alone, before the rest. Commits 2 to
4 build and pass tests each. Swift follows as one commit of about 40 lines.

TODO.md on landing: "DISCUSSION: syntax highlighting direction" goes; "Syntax
highlighting — more languages" says new code languages are tree-sitter
grammars (Swift, web next) and line highlighters are for rich formats.

## Desktop check

Open a Kotlin file, a `build.gradle.kts`, a Java file and a Rust file from
this repo: colours legible and distinct on the editor background (the new
`Syntax` colours are proposals); typing, Backspace and undo keep colours
right with no flicker on an ordinary file; a large generated file (about
50,000 lines) opens and scrolls without a stall, its colours arriving
shortly after it opens and after each edit; TOML and Markdown look as
before.

## Open questions

1. Kotlin grammar: `kotlin-ng` from crates.io (recommended) or fwcd's,
   pinned by git rev (more active, used by Neovim and Helix; a 177 MB first
   fetch). Either way the query is ours.
2. Code files offer Source only until the heading modes (recommended), or
   keep a no-op Integrated mode as DECISION.md reads today.
3. Rust in the first increment (recommended, for this repo and as the test
   reference) or Kotlin and Java only, Rust with Swift next.
4. The new syntax colours are fixed in `syntax.rs` like the existing ones;
   making them configurable would add a feature.
