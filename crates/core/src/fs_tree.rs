//! A headless, filesystem-backed tree model for the file-browser widget.
//!
//! It knows how to list directories (lazily, respecting hidden/gitignored
//! toggles via the `ignore` crate), track which directories are expanded, flatten
//! the visible hierarchy into rows for rendering, fuzzy-filter across the tree,
//! and suggest path completions. It does no rendering and touches no GUI types,
//! so it is shared by the in-pane browser and the floating directory picker and
//! is tested headless.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::fuzzy;

/// One entry as listed from disk (already filtered by the active toggles).
#[derive(Clone, Debug, PartialEq, Eq)]
struct RawEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: Option<SystemTime>,
}

/// One entry from the recursive filter-mode walk (cached across keystrokes).
#[derive(Clone, Debug, PartialEq, Eq)]
struct WalkEntry {
    name: String,
    /// Path relative to the root — the display label in filter mode.
    rel: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: Option<SystemTime>,
}

/// Git working-tree state of a file, for the git filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GitState {
    /// Tracked and changed (staged and/or in the worktree).
    modified: bool,
    /// Not tracked by git.
    untracked: bool,
}

/// The git-status filter for a browser view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFilter {
    /// No git filtering.
    All,
    /// Only tracked files with staged/worktree changes.
    Modified,
    /// Only untracked files.
    Untracked,
}

impl GitFilter {
    /// Cycle All → Modified → Untracked → All.
    pub fn next(self) -> Self {
        match self {
            GitFilter::All => GitFilter::Modified,
            GitFilter::Modified => GitFilter::Untracked,
            GitFilter::Untracked => GitFilter::All,
        }
    }

    /// A short label for a header control.
    pub fn label(self) -> &'static str {
        match self {
            GitFilter::All => "git: all",
            GitFilter::Modified => "git: modified",
            GitFilter::Untracked => "git: untracked",
        }
    }
}

/// A row in the flattened visible tree, ready to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    /// Display label: the file name in browse mode, the path relative to the root
    /// in filter mode (so nested matches are distinguishable).
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Indent level (0 at the root). Always 0 in filter mode (a flat list).
    pub depth: usize,
    /// For a directory: whether it is currently expanded.
    pub expanded: bool,
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

/// Caps on the filter-mode walk so a huge tree never stalls a keystroke.
const FILTER_WALK_CAP: usize = 20_000;
const FILTER_RESULT_CAP: usize = 500;

/// A filesystem tree rooted at a directory, with expand state and filters.
pub struct FsTree {
    root: PathBuf,
    show_hidden: bool,
    show_gitignored: bool,
    query: String,
    expanded: BTreeSet<PathBuf>,
    /// Per-directory listings, filtered by the current toggles. Cleared when a
    /// toggle or the root changes; a single dir can be dropped to force a re-read.
    cache: HashMap<PathBuf, Vec<RawEntry>>,
    /// The recursive walk for filter mode, built once and re-scored per keystroke;
    /// cleared when the root or a toggle changes.
    walk_cache: Option<Vec<WalkEntry>>,
    /// Show only files whose git status matches (directories always pass).
    git_filter: GitFilter,
    /// Show only files modified within this window (directories always pass).
    recent: Option<Duration>,
    /// Show only files with this extension (lowercase, no dot); directories pass.
    ext: Option<String>,
    /// Git status per absolute path, built lazily when a git filter is active;
    /// cleared when the root changes or on refresh.
    git_status: Option<HashMap<PathBuf, GitState>>,
    /// Memoised flattened rows; recomputed only when `dirty`.
    rows_cache: Vec<FileRow>,
    dirty: bool,
}

impl FsTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FsTree {
            root: root.into(),
            show_hidden: false,
            show_gitignored: false,
            query: String::new(),
            expanded: BTreeSet::new(),
            cache: HashMap::new(),
            walk_cache: None,
            git_filter: GitFilter::All,
            recent: None,
            ext: None,
            git_status: None,
            rows_cache: Vec::new(),
            dirty: true,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Navigate to a new root directory, resetting expand state and cache.
    pub fn set_root(&mut self, root: impl Into<PathBuf>) {
        self.root = root.into();
        self.expanded.clear();
        self.cache.clear();
        self.walk_cache = None;
        self.git_status = None;
        self.query.clear();
        self.dirty = true;
    }

    /// Move the root to its parent directory, if any. Returns whether it moved.
    pub fn go_to_parent(&mut self) -> bool {
        if let Some(parent) = self.root.parent().map(Path::to_path_buf) {
            self.set_root(parent);
            true
        } else {
            false
        }
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn set_show_hidden(&mut self, v: bool) {
        if self.show_hidden != v {
            self.show_hidden = v;
            self.cache.clear();
            self.walk_cache = None;
            self.dirty = true;
        }
    }

    pub fn show_gitignored(&self) -> bool {
        self.show_gitignored
    }

    pub fn set_show_gitignored(&mut self, v: bool) {
        if self.show_gitignored != v {
            self.show_gitignored = v;
            self.cache.clear();
            self.walk_cache = None;
            self.dirty = true;
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn set_query(&mut self, q: impl Into<String>) {
        let q = q.into();
        if self.query != q {
            self.query = q;
            self.dirty = true;
        }
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    /// Expand or collapse a directory.
    pub fn toggle_dir(&mut self, path: &Path) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
        }
        self.dirty = true;
    }

    /// Drop cached listings so the next `rows` re-reads from disk.
    pub fn refresh(&mut self) {
        self.cache.clear();
        self.walk_cache = None;
        self.git_status = None;
        self.dirty = true;
    }

    pub fn git_filter(&self) -> GitFilter {
        self.git_filter
    }

    /// Advance the git-status filter (All → Modified → Untracked → All).
    pub fn cycle_git_filter(&mut self) {
        self.git_filter = self.git_filter.next();
        self.dirty = true;
    }

    /// Whether the "recently modified" filter is on.
    pub fn recent_on(&self) -> bool {
        self.recent.is_some()
    }

    /// Toggle the "modified within `dur`" filter.
    pub fn toggle_recent(&mut self, dur: Duration) {
        self.recent = if self.recent.is_some() { None } else { Some(dur) };
        self.dirty = true;
    }

    pub fn ext_filter(&self) -> Option<&str> {
        self.ext.as_deref()
    }

    /// Restrict to files with `ext` (no dot); `None`/empty clears it.
    pub fn set_ext_filter(&mut self, ext: Option<String>) {
        self.ext = ext.filter(|e| !e.is_empty()).map(|e| e.trim_start_matches('.').to_lowercase());
        self.dirty = true;
    }

    /// The flattened visible rows. In browse mode (empty query) this is the tree
    /// of the root and its expanded descendants; in filter mode it is a flat,
    /// score-ranked list of matches walked from the root.
    pub fn rows(&mut self) -> &[FileRow] {
        if self.dirty {
            if self.git_filter != GitFilter::All {
                self.ensure_git_status();
            }
            self.rows_cache = if self.query.trim().is_empty() || self.input_is_path() {
                // A path-like input drives the completion dropdown, not a filter, so
                // the tree keeps showing the current root while the user types.
                self.browse_rows()
            } else {
                self.ensure_walk();
                self.filter_rows()
            };
            self.dirty = false;
        }
        &self.rows_cache
    }

    /// Whether the git/recent/type filters are all off.
    pub fn filters_active(&self) -> bool {
        self.git_filter != GitFilter::All || self.recent.is_some() || self.ext.is_some()
    }

    /// Whether a file (not a directory) passes the git/recent/type filters.
    /// Directories always pass so the tree stays navigable.
    fn file_passes(&self, path: &Path, mtime: Option<SystemTime>) -> bool {
        if let Some(dur) = self.recent {
            let recent = mtime
                .and_then(|t| t.elapsed().ok())
                .map(|e| e <= dur)
                .unwrap_or(false);
            if !recent {
                return false;
            }
        }
        if let Some(ext) = &self.ext {
            let matches = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case(ext))
                .unwrap_or(false);
            if !matches {
                return false;
            }
        }
        match self.git_filter {
            GitFilter::All => {}
            GitFilter::Modified => {
                if !self.git_state(path).modified {
                    return false;
                }
            }
            GitFilter::Untracked => {
                if !self.git_state(path).untracked {
                    return false;
                }
            }
        }
        true
    }

    fn git_state(&self, path: &Path) -> GitState {
        self.git_status
            .as_ref()
            .and_then(|m| m.get(path))
            .copied()
            .unwrap_or_default()
    }

    /// Run `git status --porcelain` for the repo containing the root and record a
    /// per-path state. A non-repo (or git failure) yields an empty map.
    fn ensure_git_status(&mut self) {
        if self.git_status.is_some() {
            return;
        }
        let mut map: HashMap<PathBuf, GitState> = HashMap::new();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["status", "--porcelain", "--no-renames"])
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                // Resolve the repo top-level so porcelain paths (repo-relative) map
                // to absolute paths.
                let top = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&self.root)
                    .args(["rev-parse", "--show-toplevel"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
                    .unwrap_or_else(|| self.root.clone());
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if line.len() < 4 {
                        continue;
                    }
                    let code = &line[..2];
                    let rel = line[3..].trim();
                    let abs = top.join(rel);
                    let untracked = code == "??";
                    map.insert(
                        abs,
                        GitState {
                            modified: !untracked,
                            untracked,
                        },
                    );
                }
            }
        }
        self.git_status = Some(map);
    }

    /// Whether the current input reads as a path (drives autocomplete/navigation)
    /// rather than a fuzzy filter. A leading `~`, `/`, `.` or any `/` means path.
    pub fn input_is_path(&self) -> bool {
        let q = self.query.trim_start();
        q.starts_with('~') || q.starts_with('/') || q.starts_with('.') || q.contains('/')
    }

    /// Act on the current path-like input. If it names an existing file, return it
    /// (the caller opens it). Otherwise navigate to it (the input itself if it is a
    /// directory, else its nearest existing parent) and return `None`.
    pub fn navigate_input(&mut self) -> Option<PathBuf> {
        let p = expand_tilde(self.query.trim());
        if p.is_file() {
            return Some(p);
        }
        let dir = if p.is_dir() {
            Some(p.clone())
        } else {
            p.parent().filter(|d| d.is_dir()).map(Path::to_path_buf)
        };
        if let Some(d) = dir {
            self.set_root(d);
        }
        None
    }

    /// The first path completion for the current input (for Tab-complete), with a
    /// trailing `/` when it is a directory.
    pub fn first_completion(&self) -> Option<String> {
        self.suggestions(&self.query).into_iter().next().map(|p| {
            let mut s = p.to_string_lossy().into_owned();
            if p.is_dir() {
                s.push('/');
            }
            s
        })
    }

    /// Path completions for `input`: entries in the directory `input` names (or
    /// its parent, for a partial final component), matching the partial prefix.
    /// Directories come first. Used by the path box's autocomplete.
    pub fn suggestions(&self, input: &str) -> Vec<PathBuf> {
        let expanded = expand_tilde(input);
        // Split into the directory to list and the partial final component.
        let (dir, partial): (PathBuf, String) = if input.ends_with('/') || input.is_empty() {
            (expanded.clone(), String::new())
        } else {
            match expanded.parent() {
                Some(p) if !p.as_os_str().is_empty() => (
                    p.to_path_buf(),
                    expanded
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
                _ => (expanded.clone(), String::new()),
            }
        };
        let base = if dir.as_os_str().is_empty() {
            self.root.clone()
        } else {
            dir
        };
        let mut out: Vec<(bool, PathBuf)> = Vec::new();
        if let Ok(read) = std::fs::read_dir(&base) {
            for e in read.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !partial.is_empty() && !name.to_lowercase().starts_with(&partial.to_lowercase()) {
                    continue;
                }
                if partial.is_empty() && name.starts_with('.') && !self.show_hidden {
                    continue;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                out.push((is_dir, e.path()));
            }
        }
        // Directories first, then case-insensitive by name.
        out.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.to_string_lossy().to_lowercase().cmp(&b.1.to_string_lossy().to_lowercase()))
        });
        out.into_iter().map(|(_, p)| p).collect()
    }

    /// Browse-mode rows: the root's children, recursing into expanded dirs.
    fn browse_rows(&mut self) -> Vec<FileRow> {
        let mut out = Vec::new();
        let root = self.root.clone();
        self.append_dir_rows(&root, 0, &mut out);
        out
    }

    fn append_dir_rows(&mut self, dir: &Path, depth: usize, out: &mut Vec<FileRow>) {
        let entries = self.list_dir(dir);
        for e in entries {
            if !e.is_dir && !self.file_passes(&e.path, e.mtime) {
                continue;
            }
            let expanded = e.is_dir && self.expanded.contains(&e.path);
            out.push(FileRow {
                name: e.name.clone(),
                path: e.path.clone(),
                is_dir: e.is_dir,
                depth,
                expanded,
                size: e.size,
                mtime: e.mtime,
            });
            if expanded {
                let child = e.path.clone();
                self.append_dir_rows(&child, depth + 1, out);
            }
        }
    }

    /// Populate the recursive filter walk once (respecting toggles). Bounded by
    /// [`FILTER_WALK_CAP`] so a huge tree can't stall.
    fn ensure_walk(&mut self) {
        if self.walk_cache.is_some() {
            return;
        }
        let mut entries: Vec<WalkEntry> = Vec::new();
        let mut seen = 0usize;
        for dent in self.walk_builder(&self.root).build().flatten() {
            if dent.depth() == 0 {
                continue; // the root itself
            }
            seen += 1;
            if seen > FILTER_WALK_CAP {
                break;
            }
            let path = dent.path().to_path_buf();
            let rel = path
                .strip_prefix(&self.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let name = dent.file_name().to_string_lossy().into_owned();
            let is_dir = dent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let (size, mtime) = dent
                .metadata()
                .map(|m| (m.len(), m.modified().ok()))
                .unwrap_or((0, None));
            entries.push(WalkEntry {
                name,
                rel,
                path,
                is_dir,
                size,
                mtime,
            });
        }
        self.walk_cache = Some(entries);
    }

    /// Filter-mode rows: fuzzy-rank the cached walk by each entry's **file name**
    /// (not its full path), so a directory that matches doesn't drag in all of its
    /// descendants — a deep entry matches only on its own name. The display label
    /// is the path relative to the root, so which match it is stays clear.
    fn filter_rows(&self) -> Vec<FileRow> {
        let query = self.query.trim();
        let entries = self.walk_cache.as_deref().unwrap_or(&[]);
        let mut scored: Vec<(i32, &WalkEntry)> = entries
            .iter()
            .filter(|e| e.is_dir || self.file_passes(&e.path, e.mtime))
            .filter_map(|e| fuzzy::score(query, &e.name).map(|s| (s, e)))
            .collect();
        // Highest score first; ties broken by shorter path then name.
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.rel.len().cmp(&b.1.rel.len()))
                .then_with(|| a.1.rel.cmp(&b.1.rel))
        });
        scored.truncate(FILTER_RESULT_CAP);
        scored
            .into_iter()
            .map(|(_, e)| FileRow {
                name: e.rel.clone(),
                path: e.path.clone(),
                is_dir: e.is_dir,
                depth: 0,
                expanded: false,
                size: e.size,
                mtime: e.mtime,
            })
            .collect()
    }

    /// Read one directory level, filtered by the toggles and sorted (dirs first,
    /// then case-insensitive name). Cached until a toggle/root change.
    fn list_dir(&mut self, dir: &Path) -> Vec<RawEntry> {
        if let Some(cached) = self.cache.get(dir) {
            return cached.clone();
        }
        let mut entries: Vec<RawEntry> = Vec::new();
        for dent in self
            .walk_builder(dir)
            .max_depth(Some(1))
            .build()
            .flatten()
        {
            if dent.depth() == 0 {
                continue;
            }
            let path = dent.path().to_path_buf();
            let name = dent.file_name().to_string_lossy().into_owned();
            let is_dir = dent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let (size, mtime) = dent
                .metadata()
                .map(|m| (m.len(), m.modified().ok()))
                .unwrap_or((0, None));
            entries.push(RawEntry {
                name,
                path,
                is_dir,
                size,
                mtime,
            });
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        self.cache.insert(dir.to_path_buf(), entries.clone());
        entries
    }

    /// A walk builder honouring the current toggles. `hidden(true)` *skips* hidden
    /// files, so it is the negation of `show_hidden`; likewise for gitignore.
    fn walk_builder(&self, dir: &Path) -> ignore::WalkBuilder {
        let mut b = ignore::WalkBuilder::new(dir);
        b.hidden(!self.show_hidden)
            .git_ignore(!self.show_gitignored)
            .git_global(!self.show_gitignored)
            .git_exclude(!self.show_gitignored)
            .ignore(!self.show_gitignored)
            .parents(true)
            .follow_links(false);
        b
    }
}

/// Expand a leading `~` to the home directory.
fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if input == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "ghostrealm-fstree-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn lists_and_sorts_dirs_first() {
        let d = tmpdir();
        fs::write(d.join("b.txt"), "x").unwrap();
        fs::write(d.join("a.txt"), "x").unwrap();
        fs::create_dir(d.join("zdir")).unwrap();
        let mut t = FsTree::new(&d);
        let rows = t.rows();
        assert_eq!(rows[0].name, "zdir", "directories sort before files");
        assert!(rows[0].is_dir);
        let files: Vec<&str> = rows.iter().filter(|r| !r.is_dir).map(|r| r.name.as_str()).collect();
        assert_eq!(files, vec!["a.txt", "b.txt"], "files sort case-insensitively");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn hidden_toggle_shows_dotfiles() {
        let d = tmpdir();
        fs::write(d.join(".secret"), "x").unwrap();
        fs::write(d.join("visible"), "x").unwrap();
        let mut t = FsTree::new(&d);
        assert!(t.rows().iter().all(|r| r.name != ".secret"), "hidden by default");
        t.set_show_hidden(true);
        assert!(t.rows().iter().any(|r| r.name == ".secret"), "shown when toggled");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn expanding_a_dir_reveals_children_indented() {
        let d = tmpdir();
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("sub").join("inner.txt"), "x").unwrap();
        let mut t = FsTree::new(&d);
        let sub = d.join("sub");
        assert!(t.rows().iter().all(|r| r.name != "inner.txt"), "collapsed hides children");
        t.toggle_dir(&sub);
        let rows = t.rows().to_vec();
        let inner = rows.iter().find(|r| r.name == "inner.txt").expect("child shown");
        assert_eq!(inner.depth, 1, "child is indented one level");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn query_filters_across_the_tree() {
        let d = tmpdir();
        fs::create_dir(d.join("src")).unwrap();
        fs::write(d.join("src").join("main.rs"), "x").unwrap();
        fs::write(d.join("readme.md"), "x").unwrap();
        let mut t = FsTree::new(&d);
        t.set_query("main");
        let rows = t.rows();
        assert!(rows.iter().any(|r| r.name.ends_with("main.rs")), "match found without expanding");
        assert!(rows.iter().all(|r| !r.name.contains("readme")), "non-matches excluded");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn filter_matches_names_not_ancestor_paths() {
        // A matching directory must not drag in every descendant just because its
        // name is in their path; a deep entry matches only on its own name.
        let d = tmpdir();
        fs::create_dir_all(d.join("Developer").join("xx").join("yy").join("Dev")).unwrap();
        fs::write(d.join("Developer").join("ghostrealm"), "x").unwrap();
        let mut t = FsTree::new(&d);
        t.set_query("Dev");
        let names: Vec<String> = t.rows().iter().map(|r| r.name.clone()).collect();
        assert!(names.iter().any(|n| n == "Developer"), "top-level Developer matches");
        assert!(
            names.iter().any(|n| n.ends_with("yy/Dev")),
            "the deep Dev matches on its own name"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("ghostrealm")),
            "a non-matching descendant of a matched dir is excluded"
        );
        // "Developer/xx" (name xx) should not appear despite Developer in its path.
        assert!(
            !names.iter().any(|n| n == "Developer/xx"),
            "intermediate dirs don't match on the ancestor's name"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn ext_filter_limits_to_one_type_but_keeps_dirs() {
        let d = tmpdir();
        fs::write(d.join("a.rs"), "x").unwrap();
        fs::write(d.join("b.txt"), "x").unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        let mut t = FsTree::new(&d);
        t.set_ext_filter(Some("rs".into()));
        let rows = t.rows();
        assert!(rows.iter().any(|r| r.name == "a.rs"));
        assert!(rows.iter().all(|r| r.name != "b.txt"), "other types hidden");
        assert!(rows.iter().any(|r| r.name == "sub"), "directories stay visible");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn recent_filter_gates_by_mtime() {
        let d = tmpdir();
        fs::write(d.join("new.txt"), "x").unwrap();
        let mut t = FsTree::new(&d);
        t.toggle_recent(Duration::from_secs(0));
        assert!(
            t.rows().iter().all(|r| r.name != "new.txt"),
            "a 0s window hides everything"
        );
        t.toggle_recent(Duration::from_secs(0)); // off
        t.toggle_recent(Duration::from_secs(3600));
        assert!(
            t.rows().iter().any(|r| r.name == "new.txt"),
            "a 1h window shows a just-created file"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn suggestions_complete_a_partial_component() {
        let d = tmpdir();
        fs::create_dir(d.join("alpha")).unwrap();
        fs::create_dir(d.join("apex")).unwrap();
        fs::write(d.join("other"), "x").unwrap();
        let t = FsTree::new(&d);
        let input = format!("{}/a", d.display());
        let names: Vec<String> = t
            .suggestions(&input)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"apex".to_string()));
        assert!(!names.contains(&"other".to_string()));
        fs::remove_dir_all(&d).ok();
    }
}
