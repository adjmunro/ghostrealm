//! Searching file contents under a folder: the Files sidebar's `?` mode.
//!
//! A [`SearchQuery`] compiles to one regex (a plain query is escaped). The
//! [`Searcher`] runs it on a worker thread over the files a sidebar lists:
//! a walk of the root that respects the ignore files and the hidden and
//! ignored toggles, or (for the git filter) a set of files checked against
//! the same toggles. It skips binary, non-UTF-8, and large files, and
//! streams one [`FileHits`] per file with a match. Each search has a
//! generation: starting another stops the one before, and its late results
//! are dropped.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use regex::{Regex, RegexBuilder};
use regex_syntax::ast::{Ast, ClassSet, ClassSetItem};

use crate::fs_tree::{walk_builder, Waker};

/// Files larger than this aren't searched (logs, bundles, data dumps).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Matching lines kept per file; the rest only count.
pub const LINES_PER_FILE: usize = 5;
/// The search stops after this many files with a match...
pub const MAX_FILES: usize = 500;
/// ...or this many matches in all...
pub const MAX_MATCHES: usize = 20_000;
/// ...or after walking this many entries, so a huge tree can't keep it
/// going.
pub const MAX_WALK_ENTRIES: usize = 100_000;
/// A kept line longer than this (chars) is cut to a window this long around
/// its first match, so a minified file doesn't ship megabyte lines.
pub const MAX_LINE_CHARS: usize = 400;
/// How far before its first match a cut line starts (chars).
const LINE_LEAD: usize = 80;
/// How much of a file is checked for a NUL byte, the binary-file test.
const BINARY_PROBE: usize = 8 * 1024;
/// The least time between two wakes while results stream in.
const WAKE_EVERY: Duration = Duration::from_millis(30);
/// How many lines of a file go by between checks that the search is still
/// wanted.
const LINES_PER_CHECK: usize = 256;
/// How long matching one line may take before the rest of it is given up:
/// a regex that scans to the line's end from each start (`x.*Q|x`, or a
/// greedy whole-word match rejected over and over) is quadratic in it. Far
/// above what a linear scan of a 1 MB line takes, even unoptimised.
pub const LINE_TIME_BUDGET: Duration = Duration::from_secs(1);

/// How letter case counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Case {
    /// Sensitive when the query holds an uppercase letter, else not.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

/// What to search for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub case: Case,
    /// Match only where the match's edges are word edges: an end that is a
    /// word character has no word character beside it (so `foo(` matches in
    /// `foo(x)`).
    pub whole_word: bool,
    /// `text` is a regular expression; otherwise it is literal.
    pub regex: bool,
}

impl SearchQuery {
    /// Whether matching is case-sensitive. Smart case counts the uppercase
    /// letters the query matches literally: in a regex, not those of an
    /// escape or class name (`\W`, `\p{Lu}`), as ripgrep does.
    pub fn case_sensitive(&self) -> bool {
        match self.case {
            Case::Sensitive => true,
            Case::Insensitive => false,
            Case::Smart if self.regex => {
                regex_syntax::ast::parse::Parser::new().parse(&self.text).is_ok_and(|ast| has_upper(&ast))
            }
            Case::Smart => self.text.chars().any(char::is_uppercase),
        }
    }

    /// The query as a matcher, or why the regex doesn't compile.
    pub fn matcher(&self) -> Result<Matcher, String> {
        let pattern = if self.regex { self.text.clone() } else { regex::escape(&self.text) };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive())
            .build()
            .map_err(|e| regex_error(&e))?;
        Ok(Matcher { re, whole_word: self.whole_word, line_budget: LINE_TIME_BUDGET })
    }
}

/// Whether `ast` matches an uppercase letter literally.
fn has_upper(ast: &Ast) -> bool {
    match ast {
        Ast::Literal(l) => l.c.is_uppercase(),
        Ast::ClassBracketed(c) => set_has_upper(&c.kind),
        Ast::Repetition(r) => has_upper(&r.ast),
        Ast::Group(g) => has_upper(&g.ast),
        Ast::Alternation(a) => a.asts.iter().any(has_upper),
        Ast::Concat(c) => c.asts.iter().any(has_upper),
        _ => false,
    }
}

fn set_has_upper(set: &ClassSet) -> bool {
    match set {
        ClassSet::Item(item) => item_has_upper(item),
        ClassSet::BinaryOp(op) => set_has_upper(&op.lhs) || set_has_upper(&op.rhs),
    }
}

fn item_has_upper(item: &ClassSetItem) -> bool {
    match item {
        ClassSetItem::Literal(l) => l.c.is_uppercase(),
        ClassSetItem::Range(r) => r.start.c.is_uppercase() || r.end.c.is_uppercase(),
        ClassSetItem::Bracketed(c) => set_has_upper(&c.kind),
        ClassSetItem::Union(u) => u.items.iter().any(item_has_upper),
        _ => false,
    }
}

/// A regex error's own line: the full message repeats the pattern with a
/// caret under the fault, which a one-line display can't show.
fn regex_error(e: &regex::Error) -> String {
    let text = e.to_string();
    let line = text.lines().rev().find(|l| l.starts_with("error:")).unwrap_or(&text);
    line.trim_start_matches("error:").trim().to_string()
}

/// A compiled [`SearchQuery`].
#[derive(Clone, Debug)]
pub struct Matcher {
    re: Regex,
    whole_word: bool,
    /// How long one line's matching may take ([`LINE_TIME_BUDGET`]).
    line_budget: Duration,
}

impl Matcher {
    /// The matches in `line`, as byte ranges: see [`Matches`].
    pub fn matches<'a>(&'a self, line: &'a str) -> Matches<'a> {
        Matches { matcher: self, line, at: 0, finds: 0, since: None, partial: false, current: None }
    }

    /// [`Self::matches`], giving up once `current` says the search is no
    /// longer wanted (asked before each find after the first: one find can
    /// scan the rest of a long line, so a newer search isn't kept waiting
    /// for a batch of them).
    pub fn matches_while<'a>(&'a self, line: &'a str, current: &'a dyn Fn() -> bool) -> Matches<'a> {
        Matches { current: Some(current), ..self.matches(line) }
    }

    /// Every match in `line`, collected.
    pub fn find_iter(&self, line: &str) -> Vec<Range<usize>> {
        self.matches(line).collect()
    }
}

/// A line's matches as byte ranges, left to right, none empty and none
/// overlapping. A whole-word match a word character touches is skipped,
/// and the search goes on past the word it starts in (any match starting
/// inside that word fails the same way). Once matching the line has taken
/// [`LINE_TIME_BUDGET`], or the search is no longer wanted, the line has no
/// more matches and is [`partial`](Self::partial).
pub struct Matches<'a> {
    matcher: &'a Matcher,
    line: &'a str,
    at: usize,
    /// Regex searches run, kept or not.
    finds: usize,
    /// When the first ran.
    since: Option<Instant>,
    partial: bool,
    current: Option<&'a dyn Fn() -> bool>,
}

impl Matches<'_> {
    /// Whether matching stopped before the end of the line: there may be
    /// more matches than it gave.
    pub fn partial(&self) -> bool {
        self.partial
    }

    fn give_up(&mut self) -> Option<Range<usize>> {
        self.partial = true;
        self.at = self.line.len() + 1;
        None
    }
}

impl Iterator for Matches<'_> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        let line = self.line;
        let since = *self.since.get_or_insert_with(Instant::now);
        while self.at <= line.len() {
            // Every find counts, kept or not: one can scan the rest of the
            // line.
            if self.finds > 0 {
                let unwanted = self.current.is_some_and(|c| !c());
                if unwanted || since.elapsed() > self.matcher.line_budget {
                    return self.give_up();
                }
            }
            self.finds += 1;
            let m = self.matcher.re.find_at(line, self.at)?;
            if m.is_empty() {
                self.at = next_char(line, m.start());
                continue;
            }
            if self.matcher.whole_word && !whole_word(line, m.range()) {
                self.at = past_word(line, m.start());
                continue;
            }
            self.at = m.end();
            return Some(m.range());
        }
        None
    }
}

/// The byte index after the char at `i` (past the end: one more).
fn next_char(s: &str, i: usize) -> usize {
    s[i..].chars().next().map_or(i + 1, |c| i + c.len_utf8())
}

/// The byte index after the run of word characters starting at `i`, or
/// after the char there when it isn't one.
fn past_word(s: &str, i: usize) -> usize {
    let run: usize = s[i..].chars().take_while(|c| is_word(*c)).map(char::len_utf8).sum();
    if run == 0 {
        next_char(s, i)
    } else {
        i + run
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `r` in `s` is whole words: each end that is a word character
/// has no word character beside it.
fn whole_word(s: &str, r: Range<usize>) -> bool {
    let m = &s[r.clone()];
    let first = m.chars().next().is_some_and(is_word);
    let last = m.chars().next_back().is_some_and(is_word);
    let before = s[..r.start].chars().next_back().is_some_and(is_word);
    let after = s[r.end..].chars().next().is_some_and(is_word);
    !(first && before) && !(last && after)
}

/// A matching line as kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHit {
    /// Its line number, from 0.
    pub line: usize,
    /// The column (chars) in the line that `text` starts at: 0 unless the
    /// line was cut ([`MAX_LINE_CHARS`]).
    pub start: usize,
    /// The line goes on past `text` (it was cut).
    pub cut_end: bool,
    /// The line, or the window of it kept, without its line ending.
    pub text: String,
    /// The matches in `text`, as char ranges.
    pub ranges: Vec<Range<usize>>,
}

/// One file's matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHits {
    pub path: PathBuf,
    /// Matches in the whole file.
    pub total: usize,
    /// Lines with a match in the whole file.
    pub total_lines: usize,
    /// The first [`LINES_PER_FILE`] of them.
    pub lines: Vec<LineHit>,
    /// A line's matching stopped early ([`Matches::partial`]): the totals
    /// are a floor.
    pub partial: bool,
}

impl FileHits {
    /// Whether the file has matching lines past those kept.
    pub fn more(&self) -> bool {
        self.total_lines > self.lines.len()
    }
}

/// Which files the walk searches, as the sidebar lists them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalkOptions {
    pub show_hidden: bool,
    pub show_gitignored: bool,
    /// Only these files (the git filter's), when given.
    pub only: Option<Arc<BTreeSet<PathBuf>>>,
}

/// Why a search ended before the walk did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stop {
    /// It didn't.
    #[default]
    No,
    /// At [`MAX_FILES`] or [`MAX_MATCHES`].
    Results,
    /// At [`MAX_WALK_ENTRIES`].
    Walk,
}

/// What the current search has found so far.
#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    /// In walk order: a folder's files before its subfolders, each by name.
    pub files: Vec<FileHits>,
    pub matches: usize,
    /// A file's matching stopped early ([`FileHits::partial`]): `matches`
    /// is a floor.
    pub partial: bool,
    /// The walk finished (or stopped at a cap).
    pub done: bool,
    pub stopped: Stop,
}

enum Event {
    Hits(FileHits),
    Done(Stop),
}

/// The worker's ends: jobs in, events (tagged with their search's
/// generation) out.
type Worker = (Sender<Job>, Receiver<(u64, Event)>);

struct Job {
    generation: u64,
    root: PathBuf,
    matcher: Matcher,
    walk: WalkOptions,
}

/// Searches on a worker thread, started on the first search and ended when
/// the searcher drops. One search runs at a time: starting one stops the
/// last.
pub struct Searcher {
    waker: Option<Waker>,
    worker: Option<Worker>,
    /// The generation the worker should be working on: it stops a walk once
    /// this moves past the walk's own.
    live: Arc<AtomicU64>,
    results: SearchResults,
    /// What [`Self::results`] shows while a [`Self::refresh`] runs: the
    /// results it replaces, until it is done.
    stale: Option<SearchResults>,
    /// How long one line's matching may take ([`LINE_TIME_BUDGET`]).
    line_budget: Duration,
}

impl Searcher {
    /// A searcher that calls `waker` (from its worker) when results arrive.
    pub fn new(waker: Option<Waker>) -> Self {
        let live = Arc::new(AtomicU64::new(0));
        let (results, stale, line_budget) = (SearchResults::default(), None, LINE_TIME_BUDGET);
        Searcher { waker, worker: None, live, results, stale, line_budget }
    }

    /// Search the files under `root` for `query`, dropping the last search's
    /// results. An invalid regex starts nothing and says why.
    pub fn start(&mut self, root: &Path, query: &SearchQuery, walk: WalkOptions) -> Result<(), String> {
        self.cancel();
        let mut matcher = query.matcher()?;
        matcher.line_budget = self.line_budget;
        let generation = self.live.load(Ordering::SeqCst);
        let job = Job { generation, root: root.to_path_buf(), matcher, walk };
        let sent = match &self.worker {
            Some((jobs, _)) => jobs.send(job).map_err(|e| e.0),
            None => Err(job),
        };
        if let Err(job) = sent {
            // No worker yet, or it died: start one with this job.
            let worker = spawn(self.waker.clone(), self.live.clone());
            let _ = worker.0.send(job);
            self.worker = Some(worker);
        }
        Ok(())
    }

    /// Search again (the files changed), showing the results so far until
    /// the new search is done, so a list doesn't empty and refill.
    pub fn refresh(&mut self, root: &Path, query: &SearchQuery, walk: WalkOptions) -> Result<(), String> {
        let shown = self.stale.take().unwrap_or_else(|| std::mem::take(&mut self.results));
        self.start(root, query, walk)?;
        self.stale = Some(shown);
        Ok(())
    }

    /// Stop the current search and drop its results.
    pub fn cancel(&mut self) {
        self.live.fetch_add(1, Ordering::SeqCst);
        self.results = SearchResults::default();
        self.stale = None;
    }

    /// Take in what the worker found for the current search. Returns whether
    /// the results changed.
    pub fn poll(&mut self) -> bool {
        let Some((_, events)) = &self.worker else { return false };
        let live = self.live.load(Ordering::SeqCst);
        let mut changed = false;
        while let Ok((generation, event)) = events.try_recv() {
            if generation != live {
                continue;
            }
            changed = true;
            match event {
                Event::Hits(hits) => {
                    self.results.matches += hits.total;
                    self.results.partial |= hits.partial;
                    self.results.files.push(hits);
                }
                Event::Done(stopped) => {
                    self.results.done = true;
                    self.results.stopped = stopped;
                    self.stale = None;
                }
            }
        }
        changed
    }

    /// Whether a [`Self::refresh`] is running (its old results shown).
    pub fn refreshing(&self) -> bool {
        self.stale.is_some()
    }

    /// The current search's results, or while a refresh runs, those it
    /// replaces.
    pub fn results(&self) -> &SearchResults {
        self.stale.as_ref().unwrap_or(&self.results)
    }
}

impl Drop for Searcher {
    fn drop(&mut self) {
        // Stops a walk in progress; the closed channel ends the thread.
        self.live.fetch_add(1, Ordering::SeqCst);
    }
}

fn spawn(waker: Option<Waker>, live: Arc<AtomicU64>) -> Worker {
    let (jobs, inbox) = mpsc::channel::<Job>();
    let (outbox, events) = mpsc::channel();
    std::thread::Builder::new()
        .name("file-search".into())
        .spawn(move || {
            while let Ok(mut job) = inbox.recv() {
                while let Ok(newer) = inbox.try_recv() {
                    job = newer;
                }
                if run(&job, &live, &outbox, waker.as_ref()).is_err() {
                    return;
                }
            }
        })
        .expect("spawn file-search worker");
    (jobs, events)
}

/// Paces the wakes while results stream in: the first result wakes at
/// once, later ones at most every [`WAKE_EVERY`], and one held back wakes
/// once that has passed even if no other result follows.
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    /// A result was sent since the last wake.
    pending: bool,
}

impl Throttle {
    /// A result was sent at `now`: whether to wake.
    fn sent(&mut self, now: Instant) -> bool {
        self.pending = true;
        self.due(now)
    }

    /// Whether a result held back should wake now.
    fn due(&mut self, now: Instant) -> bool {
        let wake = self.pending && self.last.is_none_or(|t| now.duration_since(t) >= WAKE_EVERY);
        if wake {
            self.last = Some(now);
            self.pending = false;
        }
        wake
    }
}

/// What to do after a file.
enum Flow {
    Go,
    /// Stop: a cap was reached.
    Stop(Stop),
    /// Stop: a newer search took over.
    Quit,
}

/// One search's run on the worker: what it found so far and how it
/// reports.
struct Run<'a> {
    job: &'a Job,
    live: &'a AtomicU64,
    out: &'a Sender<(u64, Event)>,
    waker: Option<&'a Waker>,
    throttle: Throttle,
    files: usize,
    matches: usize,
}

impl Run<'_> {
    fn current(&self) -> bool {
        self.live.load(Ordering::SeqCst) == self.job.generation
    }

    fn wake(&self) {
        if let Some(w) = self.waker {
            w();
        }
    }

    /// Before each entry: quit for a newer search, and wake for a result
    /// the throttle held back.
    fn tick(&mut self) -> bool {
        if self.throttle.due(Instant::now()) {
            self.wake();
        }
        self.current()
    }

    /// Search the file at `path` and send its hits. Errs once the searcher
    /// is gone.
    fn file(&mut self, path: &Path) -> Result<Flow, ()> {
        let live = self.live;
        let generation = self.job.generation;
        let current = move || live.load(Ordering::SeqCst) == generation;
        let hits = match search_file(path, &self.job.matcher, &current) {
            Searched::Hits(hits) => hits,
            Searched::Nothing => return Ok(Flow::Go),
            Searched::Stopped => return Ok(Flow::Quit),
        };
        self.files += 1;
        self.matches += hits.total;
        self.out.send((generation, Event::Hits(hits))).map_err(|_| ())?;
        if self.throttle.sent(Instant::now()) {
            self.wake();
        }
        Ok(match self.files >= MAX_FILES || self.matches >= MAX_MATCHES {
            true => Flow::Stop(Stop::Results),
            false => Flow::Go,
        })
    }
}

/// Search `job.root`'s files (or `job.walk.only`), sending each file's
/// hits, then `Done` unless a newer search took over. Errs once the
/// searcher is gone.
fn run(job: &Job, live: &AtomicU64, out: &Sender<(u64, Event)>, waker: Option<&Waker>) -> Result<(), ()> {
    let mut run = Run { job, live, out, waker, throttle: Throttle::default(), files: 0, matches: 0 };
    let stopped = match &job.walk.only {
        Some(only) => search_set(&mut run, only)?,
        None => search_walk(&mut run)?,
    };
    let Some(stopped) = stopped else { return Ok(()) };
    if run.current() {
        out.send((job.generation, Event::Done(stopped))).map_err(|_| ())?;
        run.wake();
    }
    Ok(())
}

/// Search the files a walk of the root lists. `None`: a newer search took
/// over.
fn search_walk(run: &mut Run) -> Result<Option<Stop>, ()> {
    let walk_opts = &run.job.walk;
    let mut walk = walk_builder(&run.job.root, walk_opts.show_hidden, walk_opts.show_gitignored);
    // A folder's files before its subfolders, so the files nearest the root
    // come first; each by name, ignoring case. The sort sees only paths, so
    // each entry's kind and key are looked up once and kept.
    let keys: Mutex<HashMap<PathBuf, (bool, String)>> = Mutex::default();
    walk.sort_by_file_path(move |a, b| {
        let mut keys = keys.lock().unwrap_or_else(|e| e.into_inner());
        let mut key = |p: &Path| {
            if let Some(k) = keys.get(p) {
                return k.clone();
            }
            let k = (p.is_dir(), lower_name(p));
            keys.insert(p.to_path_buf(), k.clone());
            k
        };
        key(a).cmp(&key(b))
    });
    for (seen, entry) in walk.build().flatten().enumerate() {
        if !run.tick() {
            return Ok(None);
        }
        if seen >= MAX_WALK_ENTRIES {
            return Ok(Some(Stop::Walk));
        }
        // Links to files are searched; links to folders aren't followed (the
        // walk doesn't), so a link can't loop.
        let is_file = match entry.file_type() {
            Some(t) if t.is_symlink() => entry.path().is_file(),
            Some(t) => t.is_file(),
            None => false,
        };
        if !is_file {
            continue;
        }
        match run.file(entry.path())? {
            Flow::Go => {}
            Flow::Stop(stop) => return Ok(Some(stop)),
            Flow::Quit => return Ok(None),
        }
    }
    Ok(Some(Stop::No))
}

/// Search the files of `only` under the root, in the walk's order: no
/// walk, so a few files in a big tree are quick. Hidden ones are left out
/// unless shown, as the walk would. `None`: a newer search took over.
fn search_set(run: &mut Run, only: &BTreeSet<PathBuf>) -> Result<Option<Stop>, ()> {
    let root = run.job.root.clone();
    let hidden = |p: &Path| {
        let rel = p.strip_prefix(&root).unwrap_or(p);
        rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    };
    let mut ignores = IgnoreFiles::default();
    let mut files: Vec<&PathBuf> = only
        .iter()
        .filter(|p| p.starts_with(&root) && (run.job.walk.show_hidden || !hidden(p)))
        .filter(|p| run.job.walk.show_gitignored || !ignores.ignored(p))
        .collect();
    files.sort_by(|a, b| walk_order(a, b));
    for path in files {
        if !run.tick() {
            return Ok(None);
        }
        if !path.is_file() {
            continue;
        }
        match run.file(path)? {
            Flow::Go => {}
            Flow::Stop(stop) => return Ok(Some(stop)),
            Flow::Quit => return Ok(None),
        }
    }
    Ok(Some(Stop::No))
}

/// The ignore files a walk honours, read once per folder: whether a file
/// is ignored, as the walk would decide, for files found without one.
/// Global git excludes aren't read.
#[derive(Default)]
struct IgnoreFiles {
    /// Per folder, its matchers in precedence order: `.ignore`, `.gitignore`,
    /// and in a repo's top folder `.git/info/exclude`.
    dirs: HashMap<PathBuf, Vec<Gitignore>>,
}

impl IgnoreFiles {
    /// Whether `path` (a file) is ignored: the nearest folder whose files
    /// say anything about it decides, up to its repo's top folder.
    fn ignored(&mut self, path: &Path) -> bool {
        for dir in path.ancestors().skip(1) {
            let matchers = self.dirs.entry(dir.to_path_buf()).or_insert_with(|| Self::read(dir));
            for m in matchers.iter() {
                match m.matched_path_or_any_parents(path, false) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
            if dir.join(".git").exists() {
                break;
            }
        }
        false
    }

    fn read(dir: &Path) -> Vec<Gitignore> {
        let one = |file: PathBuf| {
            file.is_file().then(|| {
                let mut b = GitignoreBuilder::new(dir);
                b.add(file);
                b.build().ok()
            })?
        };
        [dir.join(".ignore"), dir.join(".gitignore"), dir.join(".git").join("info").join("exclude")]
            .into_iter()
            .filter_map(one)
            .collect()
    }
}

fn lower_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Two files' order in the walk: at the first folder they differ in, the
/// one directly in it first, then by name, ignoring case.
fn walk_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    let (ca, cb): (Vec<_>, Vec<_>) = (a.components().collect(), b.components().collect());
    for i in 0..ca.len().min(cb.len()) {
        if ca[i] == cb[i] {
            continue;
        }
        let key = |c: &[std::path::Component], i: usize| {
            (i + 1 < c.len(), c[i].as_os_str().to_string_lossy().to_lowercase())
        };
        return key(&ca, i).cmp(&key(&cb, i));
    }
    ca.len().cmp(&cb.len())
}

/// What searching one file found.
enum Searched {
    Hits(FileHits),
    /// No match, or a file not searched (binary, not UTF-8, large, unreadable).
    Nothing,
    /// A newer search took over partway.
    Stopped,
}

/// Search `path`, checking every [`LINES_PER_CHECK`] lines that the search
/// is still `current`.
fn search_file(path: &Path, matcher: &Matcher, current: &dyn Fn() -> bool) -> Searched {
    let Ok(meta) = std::fs::metadata(path) else { return Searched::Nothing };
    if meta.len() > MAX_FILE_BYTES {
        return Searched::Nothing;
    }
    let Ok(bytes) = std::fs::read(path) else { return Searched::Nothing };
    if bytes.len() as u64 > MAX_FILE_BYTES || bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return Searched::Nothing;
    }
    // The editor opens only UTF-8, so a match elsewhere couldn't be shown.
    let Ok(text) = String::from_utf8(bytes) else { return Searched::Nothing };
    let mut hits = FileHits { path: path.to_path_buf(), total: 0, total_lines: 0, lines: Vec::new(), partial: false };
    for (n, line) in text.split('\n').enumerate() {
        if n % LINES_PER_CHECK == 0 && !current() {
            return Searched::Stopped;
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let mut found = matcher.matches_while(line, current);
        let first = found.next();
        if found.partial() {
            if !current() {
                return Searched::Stopped;
            }
            hits.partial = true;
        }
        let Some(first) = first else { continue };
        hits.total_lines += 1;
        if hits.lines.len() < LINES_PER_FILE {
            let (hit, count) = line_hit(n, line, first, &mut found);
            hits.lines.push(hit);
            hits.total += count;
        } else {
            hits.total += 1 + found.by_ref().count();
        }
        if found.partial() {
            if !current() {
                return Searched::Stopped;
            }
            hits.partial = true;
        }
    }
    // A file given up on is kept, matches or not: it may have more.
    match hits.total > 0 || hits.partial {
        true => Searched::Hits(hits),
        false => Searched::Nothing,
    }
}

/// Line `n` (`line`) as kept, from its first match `first` and the rest
/// `found` (byte ranges, in order), with how many matches it has: a long
/// line is cut to a window from a little before its first match, and only
/// the matches in the window are kept, as char ranges. One pass over the
/// line: each range's columns count on from the last one's.
fn line_hit(n: usize, line: &str, first: Range<usize>, found: &mut dyn Iterator<Item = Range<usize>>) -> (LineHit, usize) {
    let (start, end, cut_end, start_col) = if line.len() <= MAX_LINE_CHARS {
        (0, line.len(), false, 0)
    } else {
        let chars = line.chars().count();
        let first_col = line[..first.start].chars().count();
        let start = match chars > MAX_LINE_CHARS {
            true => first_col.saturating_sub(LINE_LEAD).min(chars - MAX_LINE_CHARS),
            false => 0,
        };
        let mut bytes = line.char_indices().map(|(b, _)| b).skip(start);
        let start_b = bytes.next().unwrap_or(line.len());
        let end_b = bytes.nth(MAX_LINE_CHARS - 1).unwrap_or(line.len());
        (start_b, end_b, end_b < line.len(), start)
    };
    let window = &line[start..end];
    // The byte (in `window`) and column a range's columns count on from.
    let (mut at, mut col) = (0, 0);
    let mut col_of = |b: usize| {
        col += window[at..b].chars().count();
        at = b;
        col
    };
    let mut ranges = Vec::new();
    let mut count = 0;
    for r in std::iter::once(first).chain(found) {
        count += 1;
        if r.start >= end || r.end <= start {
            continue;
        }
        let (s, e) = (r.start.max(start) - start, r.end.min(end) - start);
        let s = col_of(s);
        ranges.push(s..col_of(e));
    }
    let hit = LineHit { line: n, start: start_col, cut_end, text: window.to_string(), ranges };
    (hit, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> crate::test_dir::TmpDir {
        crate::test_dir::TmpDir::new("search")
    }

    fn query(text: &str) -> SearchQuery {
        SearchQuery { text: text.into(), ..SearchQuery::default() }
    }

    /// The matched text in `line`.
    fn found(q: &SearchQuery, line: &str) -> Vec<String> {
        q.matcher().unwrap().find_iter(line).into_iter().map(|r| line[r].to_string()).collect()
    }

    /// Run `q` under `root` to the end.
    fn search(root: &Path, q: &SearchQuery, walk: WalkOptions) -> SearchResults {
        let mut s = Searcher::new(None);
        s.start(root, q, walk).unwrap();
        wait(&mut s)
    }

    fn wait(s: &mut Searcher) -> SearchResults {
        let until = Instant::now() + Duration::from_secs(10);
        while !s.results().done {
            assert!(Instant::now() < until, "the search finishes");
            s.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        s.results().clone()
    }

    fn names(r: &SearchResults, root: &Path) -> Vec<String> {
        r.files.iter().map(|f| f.path.strip_prefix(root).unwrap().display().to_string()).collect()
    }

    #[test]
    fn smart_case_is_sensitive_only_with_an_uppercase_letter() {
        assert_eq!(found(&query("foo"), "Foo foo FOO"), ["Foo", "foo", "FOO"]);
        assert_eq!(found(&query("Foo"), "Foo foo FOO"), ["Foo"]);
        let regex = SearchQuery { regex: true, ..query(r"\Wfoo") };
        assert!(!regex.case_sensitive(), "an escape's letter doesn't count");
        assert_eq!(found(&regex, " FOO"), [" FOO"]);
        let escaped_slash = SearchQuery { regex: true, ..query(r"\\Foo") };
        assert!(escaped_slash.case_sensitive(), "after an escaped backslash a letter counts");
        for (pattern, sensitive) in [(r"\p{Lu}x", false), (r"[A-Z]x", true), (r"(?:a|B)", true)] {
            let q = SearchQuery { regex: true, ..query(pattern) };
            assert_eq!(q.case_sensitive(), sensitive, "{pattern}");
        }
        assert!(query(r"\W").case_sensitive(), "a plain query's every uppercase letter counts");
    }

    #[test]
    fn sensitive_and_insensitive_override_smart_case() {
        let sensitive = SearchQuery { case: Case::Sensitive, ..query("foo") };
        assert_eq!(found(&sensitive, "Foo foo"), ["foo"]);
        let insensitive = SearchQuery { case: Case::Insensitive, ..query("Foo") };
        assert_eq!(found(&insensitive, "Foo foo"), ["Foo", "foo"]);
    }

    #[test]
    fn whole_word_needs_word_edges_where_the_match_has_word_ends() {
        let q = SearchQuery { whole_word: true, ..query("ab") };
        assert_eq!(found(&q, "ab abc cab ab_ (ab) ab"), ["ab", "ab", "ab"]);
        // A query ending in a non-word char still matches.
        let q = SearchQuery { whole_word: true, ..query("a.") };
        assert_eq!(found(&q, "a. ba."), ["a."]);
        // A non-word end needs no edge beside it.
        let q = SearchQuery { whole_word: true, regex: true, ..query("-?x") };
        assert_eq!(found(&q, "a-x"), ["-x"]);
        let q = SearchQuery { whole_word: true, ..query("foo(") };
        assert_eq!(found(&q, "foo(x) afoo(y)"), ["foo("]);
        // After a match inside a word is rejected, the search goes on past
        // the word, so a long run of rejections stays linear.
        let q = SearchQuery { whole_word: true, regex: true, ..query(r"a\w*") };
        let line = format!("{} a", "ba".repeat(100_000));
        let at = Instant::now();
        assert_eq!(found(&q, &line), ["a"]);
        assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
        // Many cheap rejections stay within the budget: no match is lost.
        let q = SearchQuery { whole_word: true, ..query("a") };
        assert_eq!(found(&q, &format!("{} a", "xa ".repeat(20_000))), ["a"]);
    }

    #[test]
    fn a_line_that_rescans_itself_stops_at_the_time_budget() {
        // Each `x.*y` runs to the line's end and is rejected (an `a` before
        // its `x`); each `x.*Q|x` runs to the line's end before taking `x`.
        // Unbounded, both rescan the line per `x`.
        let line = format!("{}y", "ax ".repeat(300_000));
        for (text, whole_word) in [("x.*y", true), ("x.*Q|x", false)] {
            let q = SearchQuery { whole_word, regex: true, ..query(text) };
            let m = q.matcher().unwrap();
            let at = Instant::now();
            let mut it = m.matches(&line);
            let n = it.by_ref().count();
            assert!(it.partial(), "{text}: given up after {n}, not finished");
            assert!(at.elapsed() < Duration::from_secs(5), "{text}: {:?}", at.elapsed());
        }
        // A search no longer wanted gives up before its next find.
        let asked = std::cell::Cell::new(0);
        let current = || {
            asked.set(asked.get() + 1);
            false
        };
        // A rejected find, and a kept one: either way the next find asks.
        for (line, kept) in [("xa ".repeat(100_000), 0), ("a ".repeat(100_000), 1)] {
            asked.set(0);
            let q = SearchQuery { whole_word: true, ..query("a") };
            let m = q.matcher().unwrap();
            let mut it = m.matches_while(&line, &current);
            let n = it.by_ref().count();
            assert!(it.partial());
            assert_eq!((asked.get(), it.finds, n), (1, 1, kept), "{line:.6}…");
        }
    }

    #[test]
    fn a_file_given_up_on_is_kept_as_partial() {
        let d = tmpdir();
        fs::write(d.join("p.txt"), format!("{}y\nxy\n", "ax ".repeat(300_000))).unwrap();
        let q = SearchQuery { whole_word: true, regex: true, ..query("x.*y") };
        let r = search(&d, &q, WalkOptions::default());
        let f = &r.files[0];
        assert!(f.partial && r.partial);
        assert_eq!((f.total, f.lines[0].line), (1, 1));
        // With no match before it gave up, it is still listed.
        fs::write(d.join("p.txt"), format!("{}y\n", "ax ".repeat(300_000))).unwrap();
        let r = search(&d, &q, WalkOptions::default());
        assert_eq!((r.files.len(), r.files[0].total, r.files[0].partial), (1, 0, true));
    }

    #[test]
    fn a_newer_search_stops_a_line_that_would_not_finish_on_its_own() {
        let d = tmpdir();
        fs::write(d.join("p.txt"), "ax ".repeat(300_000)).unwrap();
        let small = tmpdir();
        fs::write(small.join("s.txt"), "a").unwrap();
        // No time budget: the line alone would take far longer than the test.
        let mut s = Searcher::new(None);
        s.line_budget = Duration::from_secs(3600);
        let q = SearchQuery { regex: true, ..query("x.*Q|x") };
        s.start(&d, &q, WalkOptions::default()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let at = Instant::now();
        s.start(&small, &query("a"), WalkOptions::default()).unwrap();
        assert_eq!(wait(&mut s).files.len(), 1);
        assert!(at.elapsed() < Duration::from_secs(5), "{:?}", at.elapsed());
    }

    #[test]
    fn a_refresh_shows_the_old_results_until_it_is_done() {
        let d = tmpdir();
        fs::write(d.join("a.txt"), "hit").unwrap();
        let mut s = Searcher::new(None);
        s.start(&d, &query("hit"), WalkOptions::default()).unwrap();
        wait(&mut s);
        fs::write(d.join("b.txt"), "hit").unwrap();
        s.refresh(&d, &query("hit"), WalkOptions::default()).unwrap();
        assert_eq!((s.results().files.len(), s.results().done), (1, true), "the old results meanwhile");
        assert!(s.refreshing());
        let until = Instant::now() + Duration::from_secs(10);
        while s.results().files.len() < 2 {
            assert!(Instant::now() < until, "the refresh finishes");
            s.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(s.results().done, "swapped in whole, once done");
    }

    #[test]
    fn a_plain_query_is_literal_and_a_regex_is_not() {
        assert_eq!(found(&query("a.c"), "abc a.c"), ["a.c"]);
        let q = SearchQuery { regex: true, ..query("a.c") };
        assert_eq!(found(&q, "abc a.c"), ["abc", "a.c"]);
        let empty = SearchQuery { regex: true, ..query("x*") };
        assert_eq!(found(&empty, "axxb"), ["xx"], "empty matches are skipped");
    }

    #[test]
    fn an_invalid_regex_is_an_error_not_a_search() {
        let q = SearchQuery { regex: true, ..query("a(b") };
        let err = q.matcher().unwrap_err();
        assert!(err.contains("unclosed"), "{err}");
        let mut s = Searcher::new(None);
        assert!(s.start(&tmpdir(), &q, WalkOptions::default()).is_err());
        assert!(!s.poll() && s.results().files.is_empty());
    }

    #[test]
    fn hits_list_matching_lines_with_char_ranges_and_cap_lines_per_file() {
        let d = tmpdir();
        let lines: Vec<String> = (0..8).map(|i| format!("é{i} hit hit")).collect();
        fs::write(d.join("a.txt"), lines.join("\r\n")).unwrap();
        fs::write(d.join("b.txt"), "nothing").unwrap();
        let r = search(&d, &query("hit"), WalkOptions::default());
        assert_eq!(names(&r, &d), ["a.txt"]);
        let f = &r.files[0];
        assert_eq!((f.total, f.total_lines, f.lines.len(), f.more()), (16, 8, LINES_PER_FILE, true));
        let want = LineHit { line: 1, start: 0, cut_end: false, text: "é1 hit hit".into(), ranges: vec![3..6, 7..10] };
        assert_eq!(f.lines[1], want);
        assert_eq!(r.matches, 16);
        assert_eq!(r.stopped, Stop::No);
    }

    #[test]
    fn a_long_line_is_cut_around_its_first_match() {
        let line = format!("{}needle{}", "x".repeat(1000), "y".repeat(1000));
        let (hit, count) = line_hit(3, &line, 1000..1006, &mut std::iter::empty());
        assert_eq!((hit.start, hit.cut_end, count), (1000 - LINE_LEAD, true, 1));
        assert_eq!(hit.text.chars().count(), MAX_LINE_CHARS);
        assert_eq!(hit.ranges, [Range { start: LINE_LEAD, end: LINE_LEAD + 6 }]);
        // Near its end, the window ends with the line.
        let (hit, _) = line_hit(0, &line, 1990..1992, &mut std::iter::empty());
        assert!(!hit.cut_end && hit.text.ends_with("yy"));
    }

    #[test]
    fn a_megabyte_line_of_matches_is_quick() {
        let d = tmpdir();
        let line = "é a".repeat(MAX_FILE_BYTES as usize / 5);
        fs::write(d.join("min.js"), &line).unwrap();
        let at = Instant::now();
        let r = search(&d, &query("a"), WalkOptions::default());
        // Tolerant: a debug build on a busy machine, not a benchmark.
        assert!(at.elapsed() < Duration::from_secs(5), "{:?}", at.elapsed());
        let hit = &r.files[0].lines[0];
        assert_eq!(r.files[0].total, MAX_FILE_BYTES as usize / 5);
        assert!(hit.cut_end && hit.ranges.len() <= MAX_LINE_CHARS);
        assert_eq!(hit.ranges[0], Range { start: 2, end: 3 });
    }

    #[test]
    fn the_walk_respects_gitignore_the_toggles_and_a_file_set_and_orders_files_first() {
        let d = tmpdir();
        fs::create_dir(d.join(".git")).unwrap();
        fs::write(d.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(d.join("ignored.txt"), "hit").unwrap();
        fs::write(d.join(".hidden"), "hit").unwrap();
        fs::create_dir(d.join("A")).unwrap();
        fs::write(d.join("A").join("x.txt"), "hit").unwrap();
        fs::write(d.join("b.txt"), "hit").unwrap();
        fs::write(d.join("Z.txt"), "hit").unwrap();
        let r = search(&d, &query("hit"), WalkOptions::default());
        assert_eq!(names(&r, &d), ["b.txt", "Z.txt", "A/x.txt"]);
        let all = WalkOptions { show_hidden: true, show_gitignored: true, only: None };
        let r = search(&d, &query("hit"), all);
        assert_eq!(names(&r, &d), [".hidden", "b.txt", "ignored.txt", "Z.txt", "A/x.txt"]);
        let only = Some(Arc::new(BTreeSet::from([d.join("Z.txt"), d.join("A").join("x.txt")])));
        let r = search(&d, &query("hit"), WalkOptions { only, ..WalkOptions::default() });
        assert_eq!(names(&r, &d), ["Z.txt", "A/x.txt"]);
        // A set is searched as the walk would order it, hidden files left
        // out unless shown; an empty one finishes at once.
        let set = [d.join("b.txt"), d.join("A").join("x.txt"), d.join(".hidden"), d.join("Z.txt")];
        let only = Some(Arc::new(BTreeSet::from(set)));
        let r = search(&d, &query("hit"), WalkOptions { only, ..WalkOptions::default() });
        assert_eq!(names(&r, &d), ["b.txt", "Z.txt", "A/x.txt"]);
        let only = Some(Arc::new(BTreeSet::new()));
        let r = search(&d, &query("hit"), WalkOptions { only, ..WalkOptions::default() });
        assert!(r.files.is_empty() && r.done);
        // The set is held to the ignore files as the walk is.
        let only = Some(Arc::new(BTreeSet::from([d.join("ignored.txt"), d.join("b.txt")])));
        let r = search(&d, &query("hit"), WalkOptions { only: only.clone(), ..WalkOptions::default() });
        assert_eq!(names(&r, &d), ["b.txt"]);
        let r = search(&d, &query("hit"), WalkOptions { only, show_gitignored: true, ..WalkOptions::default() });
        assert_eq!(names(&r, &d), ["b.txt", "ignored.txt"]);
    }

    #[test]
    fn links_to_files_are_searched_and_links_to_folders_are_not_followed() {
        let d = tmpdir();
        fs::create_dir(d.join("real")).unwrap();
        fs::write(d.join("real").join("f.txt"), "hit").unwrap();
        std::os::unix::fs::symlink(d.join("real").join("f.txt"), d.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&d, d.join("real").join("loop")).unwrap();
        let r = search(&d, &query("hit"), WalkOptions::default());
        assert_eq!(names(&r, &d), ["link.txt", "real/f.txt"]);
    }

    #[test]
    fn binary_non_utf8_and_large_files_are_skipped() {
        let d = tmpdir();
        fs::write(d.join("bin"), b"hit\0hit").unwrap();
        fs::write(d.join("latin1.txt"), b"hit \xe9").unwrap();
        let mut big = "hit\n".repeat(10).into_bytes();
        big.resize(MAX_FILE_BYTES as usize + 1, b'x');
        fs::write(d.join("big.txt"), big).unwrap();
        fs::write(d.join("ok.txt"), "hit").unwrap();
        let r = search(&d, &query("hit"), WalkOptions::default());
        assert_eq!(names(&r, &d), ["ok.txt"]);
    }

    #[test]
    fn the_search_stops_at_the_file_cap() {
        let d = tmpdir();
        for i in 0..MAX_FILES + 5 {
            fs::write(d.join(format!("f{i:04}.txt")), "hit").unwrap();
        }
        let r = search(&d, &query("hit"), WalkOptions::default());
        assert_eq!(r.files.len(), MAX_FILES);
        assert!(r.done);
        assert_eq!(r.stopped, Stop::Results);
    }

    #[test]
    fn a_newer_search_drops_the_older_ones_results() {
        let d = tmpdir();
        for i in 0..200 {
            fs::write(d.join(format!("f{i:03}.txt")), "old\nnew").unwrap();
        }
        let mut s = Searcher::new(None);
        s.start(&d, &query("old"), WalkOptions::default()).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        s.start(&d, &query("new"), WalkOptions::default()).unwrap();
        let r = wait(&mut s);
        assert_eq!(r.files.len(), 200);
        assert!(r.files.iter().all(|f| f.lines[0].text == "new"), "only the newer search's hits");
        // Cancelled, nothing more arrives.
        s.start(&d, &query("old"), WalkOptions::default()).unwrap();
        s.cancel();
        std::thread::sleep(Duration::from_millis(50));
        s.poll();
        assert!(s.results().files.is_empty() && !s.results().done);
    }

    #[test]
    fn a_result_held_back_wakes_once_the_pause_has_passed() {
        let t0 = Instant::now();
        let mut t = Throttle::default();
        assert!(t.sent(t0), "the first result wakes at once");
        assert!(!t.sent(t0 + Duration::from_millis(5)), "the next is held back");
        assert!(!t.due(t0 + Duration::from_millis(20)), "still within the pause");
        assert!(t.due(t0 + WAKE_EVERY), "a slow tail still wakes for it");
        assert!(!t.due(t0 + WAKE_EVERY * 3), "nothing left to wake for");
    }

    #[test]
    fn the_waker_runs_when_results_arrive() {
        let d = tmpdir();
        fs::write(d.join("a.txt"), "hit").unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let mut s = Searcher::new(Some(Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        })));
        s.start(&d, &query("hit"), WalkOptions::default()).unwrap();
        rx.recv_timeout(Duration::from_secs(10)).expect("woken");
    }
}
