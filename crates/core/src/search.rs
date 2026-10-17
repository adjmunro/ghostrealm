//! Searching file contents under a folder: the Files sidebar's `?` mode.
//!
//! A [`SearchQuery`] compiles to one regex (a plain query is escaped). The
//! [`Searcher`] runs it on a worker thread over the files a walk lists
//! (respecting `.gitignore` and the hidden/ignored toggles), skipping binary
//! and large files, and streams one [`FileHits`] per file with a match. Each
//! search has a generation: starting another stops the walk of the one
//! before, and its late results are dropped.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};

use crate::fs_tree::{walk_builder, Waker};

/// Files larger than this aren't searched (logs, bundles, data dumps).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Matching lines kept per file; the rest only count.
pub const LINES_PER_FILE: usize = 5;
/// The search stops after this many files with a match...
pub const MAX_FILES: usize = 500;
/// ...or this many matches in all.
pub const MAX_MATCHES: usize = 20_000;
/// A kept line longer than this (chars) is cut to a window this long around
/// its first match, so a minified file doesn't ship megabyte lines.
pub const MAX_LINE_CHARS: usize = 400;
/// How far before its first match a cut line starts (chars).
const LINE_LEAD: usize = 80;
/// How much of a file is checked for a NUL byte, the binary-file test.
const BINARY_PROBE: usize = 8 * 1024;
/// The least time between two wakes while results stream in.
const WAKE_EVERY: Duration = Duration::from_millis(30);

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
    /// Match only where no word character touches either end.
    pub whole_word: bool,
    /// `text` is a regular expression; otherwise it is literal.
    pub regex: bool,
}

impl SearchQuery {
    /// Whether matching is case-sensitive. Smart case looks past escapes in
    /// a regex, so `\W` or `\S` doesn't make it sensitive.
    pub fn case_sensitive(&self) -> bool {
        match self.case {
            Case::Sensitive => true,
            Case::Insensitive => false,
            Case::Smart => {
                let mut escaped = false;
                self.text.chars().any(|c| {
                    let upper = c.is_uppercase() && !(self.regex && escaped);
                    escaped = self.regex && c == '\\' && !escaped;
                    upper
                })
            }
        }
    }

    /// The query as a matcher, or why the regex doesn't compile.
    pub fn matcher(&self) -> Result<Matcher, String> {
        let pattern = if self.regex { self.text.clone() } else { regex::escape(&self.text) };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive())
            .build()
            .map_err(|e| regex_error(&e))?;
        Ok(Matcher { re, whole_word: self.whole_word })
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
}

impl Matcher {
    /// The matches in `line`, as byte ranges, left to right, none empty and
    /// none overlapping. Whole-word matching retries one char further on
    /// after a match a word character touches, so an overlapping match that
    /// stands alone is still found.
    pub fn find_iter(&self, line: &str) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut at = 0;
        while at <= line.len() {
            let Some(m) = self.re.find_at(line, at) else { break };
            if m.is_empty() {
                at = next_char(line, m.start());
                continue;
            }
            if self.whole_word && !stands_alone(line, m.range()) {
                at = next_char(line, m.start());
                continue;
            }
            out.push(m.range());
            at = m.end();
        }
        out
    }
}

/// The byte index after the char at `i` (past the end: one more).
fn next_char(s: &str, i: usize) -> usize {
    s[i..].chars().next().map_or(i + 1, |c| i + c.len_utf8())
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether no word character sits right before or right after `r` in `s`.
fn stands_alone(s: &str, r: Range<usize>) -> bool {
    let before = s[..r.start].chars().next_back().is_some_and(is_word);
    let after = s[r.end..].chars().next().is_some_and(is_word);
    !before && !after
}

/// A matching line as kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHit {
    /// Its line number, from 0.
    pub line: usize,
    /// The column (chars) in the line that `text` starts at: 0 unless the
    /// line was cut ([`MAX_LINE_CHARS`]).
    pub start: usize,
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
}

impl FileHits {
    /// Whether the file has matching lines past those kept.
    pub fn more(&self) -> bool {
        self.total_lines > self.lines.len()
    }
}

/// Which entries the walk lists, as the sidebar's toggles say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkOptions {
    pub show_hidden: bool,
    pub show_gitignored: bool,
}

/// What the current search has found so far.
#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    /// In walk order: a folder's files before its subfolders, each by name.
    pub files: Vec<FileHits>,
    pub matches: usize,
    /// The walk finished (or stopped at a cap).
    pub done: bool,
    /// It stopped at [`MAX_FILES`] or [`MAX_MATCHES`].
    pub capped: bool,
}

enum Event {
    Hits(FileHits),
    Done { capped: bool },
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
}

impl Searcher {
    /// A searcher that calls `waker` (from its worker) when results arrive.
    pub fn new(waker: Option<Waker>) -> Self {
        Searcher { waker, worker: None, live: Arc::new(AtomicU64::new(0)), results: SearchResults::default() }
    }

    /// Search the files under `root` for `query`, dropping the last search's
    /// results. An invalid regex starts nothing and says why.
    pub fn start(&mut self, root: &Path, query: &SearchQuery, walk: WalkOptions) -> Result<(), String> {
        self.cancel();
        let matcher = query.matcher()?;
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

    /// Stop the current search and drop its results.
    pub fn cancel(&mut self) {
        self.live.fetch_add(1, Ordering::SeqCst);
        self.results = SearchResults::default();
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
                    self.results.files.push(hits);
                }
                Event::Done { capped } => {
                    self.results.done = true;
                    self.results.capped = capped;
                }
            }
        }
        changed
    }

    pub fn results(&self) -> &SearchResults {
        &self.results
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

/// Walk `job.root`, sending each file's hits, then `Done` unless a newer
/// search took over. Errs once the searcher is gone.
fn run(job: &Job, live: &AtomicU64, out: &Sender<(u64, Event)>, waker: Option<&Waker>) -> Result<(), ()> {
    let current = || live.load(Ordering::SeqCst) == job.generation;
    let mut woke = Instant::now();
    let wake = |force: bool, woke: &mut Instant| {
        if let Some(w) = waker.filter(|_| force || woke.elapsed() >= WAKE_EVERY) {
            w();
            *woke = Instant::now();
        }
    };
    let mut walk = walk_builder(&job.root, job.walk.show_hidden, job.walk.show_gitignored);
    // A folder's files before its subfolders, so the files nearest the root
    // come first; each by name, ignoring case.
    walk.sort_by_file_path(|a, b| {
        a.is_dir().cmp(&b.is_dir()).then_with(|| {
            let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
            name(a).cmp(&name(b))
        })
    });
    let (mut files, mut matches, mut capped) = (0, 0, false);
    for entry in walk.build().flatten() {
        if !current() {
            return Ok(());
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(hits) = search_file(entry.path(), &job.matcher) else { continue };
        files += 1;
        matches += hits.total;
        out.send((job.generation, Event::Hits(hits))).map_err(|_| ())?;
        wake(false, &mut woke);
        if files >= MAX_FILES || matches >= MAX_MATCHES {
            capped = true;
            break;
        }
    }
    if current() {
        out.send((job.generation, Event::Done { capped })).map_err(|_| ())?;
        wake(true, &mut woke);
    }
    Ok(())
}

/// `path`'s matches, or `None` for a file with none, a binary or large
/// one, or one that can't be read.
fn search_file(path: &Path, matcher: &Matcher) -> Option<FileHits> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES || bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut hits = FileHits { path: path.to_path_buf(), total: 0, total_lines: 0, lines: Vec::new() };
    for (n, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let found = matcher.find_iter(line);
        if found.is_empty() {
            continue;
        }
        hits.total += found.len();
        hits.total_lines += 1;
        if hits.lines.len() < LINES_PER_FILE {
            hits.lines.push(line_hit(n, line, &found));
        }
    }
    (hits.total > 0).then_some(hits)
}

/// Line `n` (`line`) with its byte-range matches `found`, as kept: char
/// ranges, and a long line cut to a window from a little before its first
/// match.
fn line_hit(n: usize, line: &str, found: &[Range<usize>]) -> LineHit {
    let col = |b: usize| line[..b].chars().count();
    let chars = line.chars().count();
    let first = col(found[0].start);
    let start = match chars > MAX_LINE_CHARS {
        true => first.saturating_sub(LINE_LEAD).min(chars - MAX_LINE_CHARS),
        false => 0,
    };
    let end = (start + MAX_LINE_CHARS).min(chars);
    let text: String = line.chars().skip(start).take(end - start).collect();
    let ranges = found
        .iter()
        .map(|r| col(r.start)..col(r.end))
        .filter(|r| r.start < end && r.end > start)
        .map(|r| r.start.max(start) - start..r.end.min(end) - start)
        .collect();
    LineHit { line: n, start, text, ranges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "ghostrealm-search-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
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
    }

    #[test]
    fn sensitive_and_insensitive_override_smart_case() {
        let sensitive = SearchQuery { case: Case::Sensitive, ..query("foo") };
        assert_eq!(found(&sensitive, "Foo foo"), ["foo"]);
        let insensitive = SearchQuery { case: Case::Insensitive, ..query("Foo") };
        assert_eq!(found(&insensitive, "Foo foo"), ["Foo", "foo"]);
    }

    #[test]
    fn whole_word_needs_no_word_character_either_side() {
        let q = SearchQuery { whole_word: true, ..query("ab") };
        assert_eq!(found(&q, "ab abc cab ab_ (ab) ab"), ["ab", "ab", "ab"]);
        // A query ending in a non-word char still matches.
        let q = SearchQuery { whole_word: true, ..query("a.") };
        assert_eq!(found(&q, "a. ba."), ["a."]);
        // A rejected match doesn't hide an overlapping one that stands alone.
        let q = SearchQuery { whole_word: true, regex: true, ..query("-?x") };
        assert_eq!(found(&q, "a-x"), ["x"]);
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
        assert_eq!(f.lines[1], LineHit { line: 1, start: 0, text: "é1 hit hit".into(), ranges: vec![3..6, 7..10] });
        assert_eq!(r.matches, 16);
        assert!(!r.capped);
    }

    #[test]
    fn a_long_line_is_cut_around_its_first_match() {
        let line = format!("{}needle{}", "x".repeat(1000), "y".repeat(1000));
        let found = [Range { start: 1000, end: 1006 }];
        let hit = line_hit(3, &line, &found);
        assert_eq!(hit.start, 1000 - LINE_LEAD);
        assert_eq!(hit.text.chars().count(), MAX_LINE_CHARS);
        assert_eq!(hit.ranges, [Range { start: LINE_LEAD, end: LINE_LEAD + 6 }]);
    }

    #[test]
    fn the_walk_respects_gitignore_and_the_hidden_toggle_and_orders_files_first() {
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
        let all = WalkOptions { show_hidden: true, show_gitignored: true };
        let r = search(&d, &query("hit"), all);
        assert_eq!(names(&r, &d), [".hidden", "b.txt", "ignored.txt", "Z.txt", "A/x.txt"]);
    }

    #[test]
    fn binary_and_large_files_are_skipped() {
        let d = tmpdir();
        fs::write(d.join("bin"), b"hit\0hit").unwrap();
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
        assert!(r.capped && r.done);
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
