//! A headless, filesystem-backed tree model for the file-browser widget.
//!
//! It knows how to list directories (lazily, respecting hidden/gitignored
//! toggles via the `ignore` crate), track which directories are expanded, flatten
//! the visible hierarchy into rows for rendering, fuzzy-filter across the tree,
//! and suggest path completions. It does no rendering and touches no GUI types,
//! so it is shared by the in-pane file browser and the floating directory picker and
//! is tested headless.

use std::collections::{BTreeMap, BTreeSet, HashMap};
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

/// The git-status filter for a file browser view.
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

/// How `rows()` presents the subtree: the real ancestor hierarchy, or a flat set of
/// synthetic collapsible categories grouping files by one facet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsView {
    /// The real directory hierarchy (expand/collapse real folders).
    Tree,
    /// Group files by extension.
    Extension,
    /// Group files by high-level kind (Images/Video/Audio/Documents/Code/…).
    Kind,
    /// Group files by git status (Modified/Untracked/Unchanged).
    GitStatus,
    /// Group files by modified-date bucket (Today/This week/…).
    Date,
    /// Group files by size bucket.
    Size,
}

impl FsView {
    pub fn next(self) -> Self {
        match self {
            FsView::Tree => FsView::Extension,
            FsView::Extension => FsView::Kind,
            FsView::Kind => FsView::GitStatus,
            FsView::GitStatus => FsView::Date,
            FsView::Date => FsView::Size,
            FsView::Size => FsView::Tree,
        }
    }

    /// Short header label.
    pub fn label(self) -> &'static str {
        match self {
            FsView::Tree => "tree",
            FsView::Extension => "type",
            FsView::Kind => "kind",
            FsView::GitStatus => "git",
            FsView::Date => "date",
            FsView::Size => "size",
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
    /// Char indices into `name` that the query matched (for highlighting). Empty in
    /// browse mode.
    pub match_indices: Vec<usize>,
    /// A synthetic collapsible category (grouped views), not a real filesystem
    /// entry. Rendered like a directory but toggled by `key`, not `path`.
    pub is_category: bool,
    /// The toggle key for a category row (empty for real entries).
    pub key: String,
    /// A dim ancestor prefix shown before the name to disambiguate same-named files
    /// in a grouped view (empty otherwise).
    pub prefix: String,
}

/// Caps on the filter-mode walk so a huge tree never stalls a keystroke.
const FILTER_WALK_CAP: usize = 20_000;
const FILTER_RESULT_CAP: usize = 500;

/// A filesystem tree rooted at a directory, with expand state and filters.
pub struct FsTree {
    root: PathBuf,
    show_hidden: bool,
    show_gitignored: bool,
    /// List directories only (files never appear, in any mode).
    dirs_only: bool,
    query: String,
    view: FsView,
    expanded: BTreeSet<PathBuf>,
    /// Expanded synthetic categories (grouped views), keyed by category id.
    expanded_cats: BTreeSet<String>,
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
            dirs_only: false,
            query: String::new(),
            view: FsView::Tree,
            expanded: BTreeSet::new(),
            expanded_cats: BTreeSet::new(),
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

    pub fn dirs_only(&self) -> bool {
        self.dirs_only
    }

    pub fn set_dirs_only(&mut self, v: bool) {
        if self.dirs_only != v {
            self.dirs_only = v;
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

    pub fn view(&self) -> FsView {
        self.view
    }

    pub fn set_view(&mut self, view: FsView) {
        if self.view != view {
            self.view = view;
            self.dirty = true;
        }
    }

    /// Advance to the next view mode (tree → type → kind → git → date → size → …).
    pub fn cycle_view(&mut self) {
        self.view = self.view.next();
        self.dirty = true;
    }

    /// Expand/collapse a synthetic category in a grouped view.
    pub fn toggle_category(&mut self, key: &str) {
        if !self.expanded_cats.remove(key) {
            self.expanded_cats.insert(key.to_string());
        }
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
            if self.git_filter != GitFilter::All || self.view == FsView::GitStatus {
                self.ensure_git_status();
            }
            self.rows_cache = if self.view != FsView::Tree {
                // Grouped views ignore the query; the view IS the navigation.
                self.ensure_walk();
                self.grouped_rows(self.view)
            } else if self.query.trim().is_empty() || self.input_is_path() {
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

    /// Whether a file (not a directory) passes the git/recent/type filters (none
    /// does when listing directories only). Directories always pass so the tree
    /// stays navigable.
    fn file_passes(&self, path: &Path, mtime: Option<SystemTime>) -> bool {
        if self.dirs_only {
            return false;
        }
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
    /// rather than a fuzzy/wildcard filter. Path if it starts with `~`, `/`, `./`,
    /// `../`, is exactly `.`/`..`, or contains a `/`. A bare dotted name like
    /// `.kt` stays a filter.
    pub fn input_is_path(&self) -> bool {
        let q = self.query.trim();
        q.starts_with('~')
            || q.starts_with('/')
            || q.starts_with("./")
            || q.starts_with("../")
            || q == "."
            || q == ".."
            || q.contains('/')
    }

    /// Resolve a typed path against this tree's root: `~`/`/` are home/absolute,
    /// everything else (including `.`, `..`, `foo`, `foo/bar`) is relative to the
    /// current root — not the process's working directory.
    pub fn resolve_path(&self, input: &str) -> PathBuf {
        let t = input.trim();
        if let Some(rest) = t.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(rest);
            }
        }
        if t == "~" {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home);
            }
        }
        let p = Path::new(t);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(t)
        }
    }

    /// Act on the current path-like input. If it names an existing file, return it
    /// (the caller opens it). Otherwise navigate to it (the input itself if it is a
    /// directory, else its nearest existing parent) and return `None`. The new root
    /// is canonicalised so `.`/`..` segments don't linger in the display.
    pub fn navigate_input(&mut self) -> Option<PathBuf> {
        let p = self.resolve_path(&self.query);
        if p.is_file() {
            return Some(p);
        }
        let dir = if p.is_dir() {
            Some(p.clone())
        } else {
            p.parent().filter(|d| d.is_dir()).map(Path::to_path_buf)
        };
        if let Some(d) = dir {
            let clean = std::fs::canonicalize(&d).unwrap_or(d);
            self.set_root(clean);
        }
        None
    }

    /// The full completions for the current input (for Tab / clicking), each with a
    /// trailing `/` when it is a directory. Ordered directories first.
    pub fn completions(&self) -> Vec<String> {
        self.suggestions(&self.query)
            .into_iter()
            .map(|p| {
                let mut s = p.to_string_lossy().into_owned();
                if p.is_dir() {
                    s.push('/');
                }
                s
            })
            .collect()
    }

    /// Path completions for `input`: entries in the directory `input` names (or its
    /// parent, for a partial final component), matching the partial prefix. The
    /// listed directory is resolved against the root. Directories come first.
    pub fn suggestions(&self, input: &str) -> Vec<PathBuf> {
        let t = input.trim_end_matches(' ');
        // Split the raw input into the directory part and the partial final name. A
        // final `.`/`..` component is a directory reference to list, not a prefix to
        // complete (so `../..` lists the grandparent, not entries named `..`).
        let (dir_part, partial): (String, String) = if t.ends_with('/') {
            (t.to_string(), String::new())
        } else if let Some(i) = t.rfind('/') {
            let last = &t[i + 1..];
            if last == "." || last == ".." {
                (format!("{t}/"), String::new())
            } else {
                (t[..=i].to_string(), last.to_string())
            }
        } else if t == "." || t == ".." || t == "~" {
            (t.to_string(), String::new())
        } else {
            (String::new(), t.to_string())
        };
        let base = if dir_part.is_empty() {
            self.root.clone()
        } else {
            self.resolve_path(&dir_part)
        };
        let partial_lower = partial.to_lowercase();
        let mut out: Vec<(bool, PathBuf)> = Vec::new();
        if let Ok(read) = std::fs::read_dir(&base) {
            for e in read.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !partial.is_empty() && !name.to_lowercase().starts_with(&partial_lower) {
                    continue;
                }
                if partial.is_empty() && name.starts_with('.') && !self.show_hidden {
                    continue;
                }
                let is_dir = is_dir_following_links(e.file_type().ok(), &e.path());
                if self.dirs_only && !is_dir {
                    continue;
                }
                out.push((is_dir, e.path()));
            }
        }
        // Directories first, then case-insensitive by name.
        out.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                a.1.to_string_lossy()
                    .to_lowercase()
                    .cmp(&b.1.to_string_lossy().to_lowercase())
            })
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
                match_indices: Vec::new(),
                is_category: false,
                key: String::new(),
                prefix: String::new(),
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
            let is_dir = is_dir_following_links(dent.file_type(), dent.path());
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
        let mut scored: Vec<(i32, Vec<usize>, &WalkEntry)> = self
            .walk_cache
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|e| e.is_dir || self.file_passes(&e.path, e.mtime))
            .filter_map(|e| fuzzy::match_smart(query, &e.name).map(|(s, idx)| (s, idx, e)))
            .collect();
        // Highest score first; ties broken by shorter path then name.
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.2.rel.len().cmp(&b.2.rel.len()))
                .then_with(|| a.2.rel.cmp(&b.2.rel))
        });
        scored.truncate(FILTER_RESULT_CAP);
        scored
            .into_iter()
            .map(|(_, name_idx, e)| {
                // Match indices are within the leaf name; shift onto the displayed
                // relative path (name is its final component).
                let offset = e.rel.chars().count().saturating_sub(e.name.chars().count());
                let match_indices = name_idx.into_iter().map(|i| i + offset).collect();
                FileRow {
                    name: e.rel.clone(),
                    path: e.path.clone(),
                    is_dir: e.is_dir,
                    depth: 0,
                    expanded: false,
                    size: e.size,
                    mtime: e.mtime,
                    match_indices,
                    is_category: false,
                    key: String::new(),
                    prefix: String::new(),
                }
            })
            .collect()
    }

    /// Grouped-view rows: synthetic collapsible categories over the recursive walk.
    /// Only files are grouped (that's the point — jump to files by facet), honouring
    /// the file filters. Expanded categories list their files, with same-named files
    /// disambiguated by a dim ancestor prefix.
    fn grouped_rows(&self, view: FsView) -> Vec<FileRow> {
        let files: Vec<&WalkEntry> = self
            .walk_cache
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|e| !e.is_dir && self.file_passes(&e.path, e.mtime))
            .collect();
        let mut groups: BTreeMap<(i64, String), Vec<&WalkEntry>> = BTreeMap::new();
        for e in files {
            let (order, id) = self.category_of(view, e);
            groups.entry((order, id)).or_default().push(e);
        }
        let mut out = Vec::new();
        for ((_, id), mut group) in groups {
            let expanded = self.expanded_cats.contains(&id);
            out.push(FileRow {
                name: format!("{id}  ({})", group.len()),
                path: self.root.clone(),
                is_dir: true,
                depth: 0,
                expanded,
                size: 0,
                mtime: None,
                match_indices: Vec::new(),
                is_category: true,
                key: id.clone(),
                prefix: String::new(),
            });
            if !expanded {
                continue;
            }
            group.sort_by(|a, b| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.rel.cmp(&b.rel))
            });
            let prefixes = disambiguate(&group);
            for (e, prefix) in group.into_iter().zip(prefixes) {
                out.push(FileRow {
                    name: e.name.clone(),
                    path: e.path.clone(),
                    is_dir: false,
                    depth: 1,
                    expanded: false,
                    size: e.size,
                    mtime: e.mtime,
                    match_indices: Vec::new(),
                    is_category: false,
                    key: String::new(),
                    prefix,
                });
            }
        }
        out
    }

    /// The (sort-order, category-label) a file falls into for `view`.
    fn category_of(&self, view: FsView, e: &WalkEntry) -> (i64, String) {
        match view {
            FsView::Extension => {
                let ext = Path::new(&e.name)
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.to_lowercase());
                (0, ext.map(|x| format!(".{x}")).unwrap_or_else(|| "(no extension)".into()))
            }
            FsView::Kind => {
                let (o, k) = kind_of(&e.name);
                (o, k.to_string())
            }
            FsView::GitStatus => {
                let st = self.git_state(&e.path);
                if st.modified {
                    (0, "Modified".into())
                } else if st.untracked {
                    (1, "Untracked".into())
                } else {
                    (2, "Unchanged".into())
                }
            }
            FsView::Date => {
                let (o, l) = date_bucket(e.mtime);
                (o, l.into())
            }
            FsView::Size => {
                let (o, l) = size_bucket(e.size);
                (o, l.into())
            }
            FsView::Tree => (0, String::new()),
        }
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
            let is_dir = is_dir_following_links(dent.file_type(), dent.path());
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

/// Whether an entry is a directory, resolving a symlink to its target: listings
/// don't follow links, so a link's own type never says "directory".
fn is_dir_following_links(file_type: Option<std::fs::FileType>, path: &Path) -> bool {
    match file_type {
        Some(t) if t.is_symlink() => path.is_dir(),
        Some(t) => t.is_dir(),
        None => false,
    }
}

/// For each file in a group, the dim ancestor segment that disambiguates same-named
/// files (empty when the name is already unique in the group). Uses the first depth,
/// counting up from the leaf, at which the same-named files' ancestor segments differ
/// (e.g. two `Foo.kt` sharing `a/b/c` climb to the module: `source` vs `network`).
fn disambiguate(group: &[&WalkEntry]) -> Vec<String> {
    let mut prefixes = vec![String::new(); group.len()];
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, e) in group.iter().enumerate() {
        by_name.entry(e.name.as_str()).or_default().push(i);
    }
    for (_, idxs) in by_name {
        if idxs.len() < 2 {
            continue;
        }
        // Ancestor components (root→parent), leaf dropped, per clashing file.
        let comps: Vec<Vec<&str>> = idxs
            .iter()
            .map(|&i| {
                let mut c: Vec<&str> = group[i].rel.split('/').collect();
                c.pop();
                c
            })
            .collect();
        let maxd = comps.iter().map(|c| c.len()).max().unwrap_or(0);
        let mut chosen = None;
        for d in 1..=maxd {
            let first = comps[0].len().checked_sub(d).map(|i| comps[0][i]);
            let differs = comps
                .iter()
                .any(|c| c.len().checked_sub(d).map(|i| c[i]) != first);
            if differs {
                chosen = Some(d);
                break;
            }
        }
        if let Some(d) = chosen {
            for (k, &gi) in idxs.iter().enumerate() {
                let c = &comps[k];
                if let Some(seg) = c.len().checked_sub(d).map(|i| c[i]) {
                    prefixes[gi] = seg.to_string();
                }
            }
        }
    }
    prefixes
}

/// A file's high-level kind (sort order, label) from its extension.
fn kind_of(name: &str) -> (i64, &'static str) {
    let ext = Path::new(name)
        .extension()
        .and_then(|x| x.to_str())
        .map(|x| x.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "tiff" | "heic" => {
            (0, "Images")
        }
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "wmv" | "flv" => (1, "Video"),
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "opus" | "aiff" => (2, "Audio"),
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "tex" | "pages" => (3, "Documents"),
        "rs" | "kt" | "kts" | "java" | "c" | "h" | "cpp" | "hpp" | "py" | "js" | "ts" | "tsx"
        | "jsx" | "go" | "rb" | "swift" | "sh" | "zsh" | "lua" | "zig" | "cs" | "php" | "html"
        | "css" | "scss" => (4, "Code"),
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" => (5, "Archives"),
        "json" | "toml" | "yaml" | "yml" | "xml" | "csv" | "tsv" | "sqlite" | "db" | "ini"
        | "lock" => (6, "Data"),
        _ => (7, "Other"),
    }
}

/// A modified-time bucket (sort order, label).
fn date_bucket(mtime: Option<SystemTime>) -> (i64, &'static str) {
    let Some(elapsed) = mtime.and_then(|t| t.elapsed().ok()) else {
        return (5, "Unknown");
    };
    let secs = elapsed.as_secs();
    const DAY: u64 = 86_400;
    if secs < DAY {
        (0, "Today")
    } else if secs < 7 * DAY {
        (1, "This week")
    } else if secs < 30 * DAY {
        (2, "This month")
    } else if secs < 365 * DAY {
        (3, "This year")
    } else {
        (4, "Older")
    }
}

/// A size bucket (sort order, label).
fn size_bucket(size: u64) -> (i64, &'static str) {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if size < 10 * KB {
        (0, "Tiny (<10 KB)")
    } else if size < MB {
        (1, "Small (<1 MB)")
    } else if size < 100 * MB {
        (2, "Large (<100 MB)")
    } else {
        (3, "Huge (≥100 MB)")
    }
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
    fn symlinked_dirs_list_and_expand_as_dirs() {
        let d = tmpdir();
        fs::create_dir(d.join("real")).unwrap();
        fs::write(d.join("real").join("inside.txt"), "x").unwrap();
        fs::write(d.join("file.txt"), "x").unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
        std::os::unix::fs::symlink(d.join("file.txt"), d.join("filelink")).unwrap();
        let mut t = FsTree::new(&d);
        let kind = |t: &mut FsTree, name: &str| t.rows().iter().find(|r| r.name == name).map(|r| r.is_dir);
        assert_eq!(kind(&mut t, "link"), Some(true), "a symlink to a dir is a dir");
        assert_eq!(kind(&mut t, "filelink"), Some(false), "a symlink to a file is a file");
        t.toggle_dir(&d.join("link"));
        let names: Vec<String> = t.rows().iter().map(|r| r.name.clone()).collect();
        assert!(names.contains(&"inside.txt".to_string()), "expanding a linked dir lists it: {names:?}");
        assert_eq!(t.suggestions("li"), vec![d.join("link")]);
        t.set_query("lin");
        assert_eq!(kind(&mut t, "link"), Some(true), "filter results agree");
    }

    #[test]
    fn dirs_only_hides_files_everywhere() {
        let d = tmpdir();
        fs::create_dir(d.join("adir")).unwrap();
        fs::write(d.join("adir").join("afile.txt"), "x").unwrap();
        fs::write(d.join("alpha.txt"), "x").unwrap();
        let mut t = FsTree::new(&d);
        t.set_dirs_only(true);
        t.toggle_dir(&d.join("adir"));
        let names = |t: &mut FsTree| t.rows().iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&mut t), vec!["adir"], "browse: the expanded dir lists no files");
        t.set_query("a");
        assert_eq!(names(&mut t), vec!["adir"], "filter: only directories match");
        assert_eq!(t.suggestions("a"), vec![d.join("adir")], "completions: directories only");
        t.set_query("");
        t.set_view(FsView::Extension);
        assert!(names(&mut t).is_empty(), "grouped views group files, so none");
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
    fn relative_input_resolves_against_root_not_cwd() {
        let d = tmpdir();
        fs::create_dir(d.join("sub")).unwrap();
        let t = FsTree::new(&d);
        // "." lists the root's own children (not the process working directory).
        let names: Vec<String> = t
            .suggestions(".")
            .into_iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert!(names.contains(&"sub".to_string()));
    }

    #[test]
    fn navigate_dotdot_goes_to_the_parent_of_root() {
        let d = tmpdir();
        fs::create_dir(d.join("child")).unwrap();
        let mut t = FsTree::new(d.join("child"));
        t.set_query("..");
        assert!(t.navigate_input().is_none());
        assert_eq!(t.root(), std::fs::canonicalize(&d).unwrap());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn wildcard_query_filters_by_extension() {
        let d = tmpdir();
        fs::write(d.join("main.kt"), "x").unwrap();
        fs::write(d.join("build.gradle.kts"), "x").unwrap();
        fs::write(d.join("readme.md"), "x").unwrap();
        let mut t = FsTree::new(&d);
        t.set_query("*.md");
        let names: Vec<String> = t.rows().iter().map(|r| r.name.clone()).collect();
        assert!(names.iter().any(|n| n.ends_with("readme.md")));
        assert!(names.iter().all(|n| !n.ends_with(".kt")), "only .md matches");
        // `*.k` is unanchored, so it matches .kt (contains ".k").
        t.set_query("*.k");
        assert!(t.rows().iter().any(|r| r.name.ends_with("main.kt")));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn extension_view_groups_and_expands() {
        let d = tmpdir();
        fs::create_dir(d.join("a")).unwrap();
        fs::write(d.join("a").join("main.rs"), "x").unwrap();
        fs::write(d.join("lib.rs"), "x").unwrap();
        fs::write(d.join("readme.md"), "x").unwrap();
        let mut t = FsTree::new(&d);
        t.set_view(FsView::Extension);
        let rows = t.rows().to_vec();
        assert!(rows.iter().any(|r| r.is_category && r.name.starts_with(".rs")));
        assert!(rows.iter().any(|r| r.is_category && r.name.starts_with(".md")));
        assert!(rows.iter().all(|r| r.is_category), "collapsed: only categories");
        t.toggle_category(".rs");
        let rows = t.rows().to_vec();
        assert!(rows.iter().any(|r| !r.is_category && r.name == "main.rs"));
        assert!(rows.iter().any(|r| !r.is_category && r.name == "lib.rs"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn grouped_view_disambiguates_same_named_files() {
        let d = tmpdir();
        fs::create_dir_all(d.join("source").join("a").join("b")).unwrap();
        fs::create_dir_all(d.join("network").join("a").join("b")).unwrap();
        fs::write(d.join("source").join("a").join("b").join("Foo.kt"), "x").unwrap();
        fs::write(d.join("network").join("a").join("b").join("Foo.kt"), "x").unwrap();
        let mut t = FsTree::new(&d);
        t.set_view(FsView::Extension);
        t.toggle_category(".kt");
        let rows = t.rows().to_vec();
        let prefixes: Vec<&str> = rows
            .iter()
            .filter(|r| r.name == "Foo.kt")
            .map(|r| r.prefix.as_str())
            .collect();
        assert_eq!(prefixes.len(), 2);
        assert!(prefixes.contains(&"source"), "got {prefixes:?}");
        assert!(prefixes.contains(&"network"), "got {prefixes:?}");
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
