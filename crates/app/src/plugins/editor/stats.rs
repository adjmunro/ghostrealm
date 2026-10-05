//! The ribbon's file statistic, cycled by clicking: lines, words, bytes, and —
//! for a file git tracks — lines added and deleted since `HEAD` (unsaved edits
//! included).

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use ghostrealm_core::{git, line_diff};

use super::buffer::{split_lines, EditorBuffer};
use crate::plugin::theme;

/// Which statistic the ribbon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stat {
    Lines,
    Words,
    Bytes,
    Changes,
}

impl Stat {
    fn label(self) -> &'static str {
        match self {
            Stat::Lines => "lines",
            Stat::Words => "words",
            Stat::Bytes => "bytes",
            Stat::Changes => "changes since the last commit",
        }
    }
}

pub struct Stats {
    shown: Stat,
    /// The file's lines at `HEAD`; `None` when untracked (or not fetched yet).
    head: Option<Vec<String>>,
    /// A `HEAD` fetch in flight on a worker thread.
    pending: Option<Receiver<Option<String>>>,
    /// Values keyed by the buffer's edit count they were computed at.
    words: Option<(u64, usize)>,
    changes: Option<(u64, (usize, usize))>,
}

impl Stats {
    pub fn new() -> Self {
        Stats { shown: Stat::Lines, head: None, pending: None, words: None, changes: None }
    }

    #[cfg(test)]
    pub fn shown(&self) -> Stat {
        self.shown
    }

    /// Fetch `path`'s committed text in the background (on open and after each
    /// save, since a commit may have moved `HEAD`).
    pub fn fetch_head(&mut self, path: Option<PathBuf>) {
        let Some(path) = path else {
            self.pending = None;
            self.set_head(None);
            return;
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(git::head_text(&path));
        });
        self.pending = Some(rx);
    }

    /// Take a finished `HEAD` fetch.
    pub fn poll(&mut self) {
        if let Some(Ok(text)) = self.pending.as_ref().map(|rx| rx.try_recv()) {
            self.pending = None;
            self.set_head(text);
        }
    }

    fn set_head(&mut self, text: Option<String>) {
        self.head = text.map(|t| split_lines(&t));
        self.changes = None;
        if self.head.is_none() && self.shown == Stat::Changes {
            self.shown = Stat::Lines;
        }
    }

    /// The statistic after `stat`: changes only for a tracked file.
    fn after(&self, stat: Stat) -> Stat {
        match stat {
            Stat::Lines => Stat::Words,
            Stat::Words => Stat::Bytes,
            Stat::Bytes if self.head.is_some() => Stat::Changes,
            Stat::Bytes | Stat::Changes => Stat::Lines,
        }
    }

    /// Show the next statistic.
    pub fn cycle(&mut self) {
        self.poll();
        self.shown = self.after(self.shown);
    }

    /// The tooltip: what a click shows next.
    pub fn tip(&self) -> String {
        format!("Click for {}", self.after(self.shown).label())
    }

    /// The shown statistic as coloured spans.
    pub fn spans(&mut self, buf: &EditorBuffer) -> Vec<(String, [u8; 3])> {
        let one = |s: String| vec![(s, theme::TEXT_DIM)];
        match self.shown {
            Stat::Lines => one(count(buf.lines.len(), "line", "lines")),
            Stat::Words => {
                let n = match self.words {
                    Some((at, n)) if at == buf.edits => n,
                    _ => {
                        let n = buf.lines.iter().map(|l| l.split_whitespace().count()).sum();
                        self.words = Some((buf.edits, n));
                        n
                    }
                };
                one(count(n, "word", "words"))
            }
            Stat::Bytes => {
                let n = buf.lines.iter().map(|l| l.len()).sum::<usize>() + buf.lines.len().saturating_sub(1);
                one(count(n, "byte", "bytes"))
            }
            Stat::Changes => {
                let Some(head) = &self.head else {
                    return one(String::new());
                };
                let (added, deleted) = match self.changes {
                    Some((at, c)) if at == buf.edits => c,
                    _ => {
                        let c = line_diff::line_changes(head, &buf.lines);
                        self.changes = Some((buf.edits, c));
                        c
                    }
                };
                vec![
                    (format!("+{}", group(added)), theme::ADDED),
                    ("  ".to_string(), theme::TEXT_DIM),
                    (format!("\u{2212}{}", group(deleted)), theme::DELETED),
                ]
            }
        }
    }
}

/// `n` with its unit, singular for one: "1 line", "1,204 lines".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", group(n), if n == 1 { one } else { many })
}

/// `n` with thousands separators.
fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: Vec<(String, [u8; 3])>) -> String {
        spans.into_iter().map(|(s, _)| s).collect()
    }

    #[test]
    fn cycles_lines_words_bytes_and_skips_changes_when_untracked() {
        let mut b = EditorBuffer::scratch();
        b.insert_str("hello big\nworld");
        let mut s = Stats::new();
        assert_eq!(text(s.spans(&b)), "2 lines");
        assert_eq!(s.tip(), "Click for words");
        s.cycle();
        assert_eq!(text(s.spans(&b)), "3 words");
        s.cycle();
        assert_eq!(text(s.spans(&b)), "15 bytes");
        s.cycle();
        assert_eq!(s.shown(), Stat::Lines, "no git: back to lines");
    }

    #[test]
    fn a_tracked_file_adds_changes_since_head() {
        let mut b = EditorBuffer::scratch();
        b.insert_str("a\nB\nc\nd");
        let mut s = Stats::new();
        s.set_head(Some("a\nb\nc\n".into()));
        for _ in 0..3 {
            s.cycle();
        }
        assert_eq!(s.shown(), Stat::Changes);
        assert_eq!(text(s.spans(&b)), "+2  \u{2212}1");
        s.set_head(None);
        assert_eq!(s.shown(), Stat::Lines, "untracked again: off the changes view");
    }

    #[test]
    fn fetches_head_in_the_background() {
        use std::process::Command;
        let dir = crate::plugin::testing::tmpdir("stats-git");
        let git = |args: &[&str]| {
            Command::new("git").arg("-C").arg(&dir).args(args).output().is_ok_and(|o| o.status.success())
        };
        if !git(&["init", "-q"]) {
            return; // no git here
        }
        let file = dir.join("f.txt");
        std::fs::write(&file, "x\n").unwrap();
        assert!(git(&["add", "f.txt"]));
        assert!(git(&["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "i"]));
        let mut s = Stats::new();
        s.fetch_head(Some(file));
        let start = std::time::Instant::now();
        while s.head.is_none() && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
            s.poll();
        }
        assert_eq!(s.head, Some(vec!["x".to_string()]));
    }

    #[test]
    fn counts_read_with_separators_and_singulars() {
        assert_eq!(count(1, "line", "lines"), "1 line");
        assert_eq!(count(1204, "line", "lines"), "1,204 lines");
        assert_eq!(group(1_000_000), "1,000,000");
        assert_eq!(group(999), "999");
    }
}
