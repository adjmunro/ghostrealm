//! The file commands (`file.*`) behind the file and folder context menu, and
//! the same actions for "the focused file" from the palette and the agent
//! channel. Each acts on a path argument, else on the focused view's
//! [`subject`](crate::plugin::View::subject).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ghostrealm_core::{git, ArgKind, ArgSpec, CmdError, CmdOutcome, CommandMeta, Registry};

use super::{AppState, Confirm, Confirmed, SurfaceId};
use crate::file_jobs::{self, Done, FileJob, Output};
use crate::file_menu::{self, FileAction, FileTarget, MenuEntry, MenuEnv};
use crate::file_ops;
use crate::plugins::editor;
use crate::plugins::file_browser::{self, FileBrowserView};
use crate::plugins::terminal;

/// How long a file command waits for its job on the worker before returning.
/// A job done by then is applied before the command returns, as if it ran on
/// the UI thread, so a quick paste or Delete shows no frame in between; a
/// slower one is applied when it finishes.
pub(super) const INLINE_WAIT: Duration = Duration::from_millis(30);

/// How a file job that finished after its command returned turned out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileReport {
    /// The command it did the work of (`file.paste`).
    pub command: &'static str,
    /// What the command would have said, or why it failed.
    pub result: Result<String, String>,
    /// The operation went ahead but not wholly (a move that left part of the
    /// original behind): `result` says what, for the user to see.
    pub warning: bool,
}

/// What Cut or Copy picked, for Paste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileClip {
    pub path: PathBuf,
    /// Cut: pasting moves the entry instead of copying it.
    pub cut: bool,
}

impl AppState {
    /// Use `system` for the Trash, the file manager, and default apps.
    pub fn with_system(mut self, system: Box<dyn file_ops::System>) -> Self {
        self.system = system;
        self
    }

    /// The file menu for `target`.
    pub fn file_menu(&self, target: &FileTarget) -> Vec<MenuEntry> {
        let env = MenuEnv {
            in_git: file_ops::containing_repo(&target.path).is_ok(),
            can_paste: self.file_clip.is_some(),
            reveal: cfg!(target_os = "macos"),
        };
        file_menu::entries(target, env)
    }

    /// What Cut or Copy last picked.
    pub fn file_clip(&self) -> Option<&FileClip> {
        self.file_clip.as_ref()
    }

    /// The text a command wants on the system clipboard, once.
    pub fn take_clipboard_text(&mut self) -> Option<String> {
        self.clipboard_out.take()
    }

    /// The file or folder the focused view is about.
    pub fn focused_subject(&self) -> Option<PathBuf> {
        self.views.get(&self.focused_surface()?)?.subject()
    }

    /// A path argument: absolute as given, relative from the active
    /// workspace's folder. Never from the process's own directory, which
    /// nothing in the app shows (it is `/` when launched from Finder), and
    /// which open views' paths wouldn't match.
    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(path);
        }
        let root = self.active().and_then(|vt| self.vtab_dir(vt));
        Ok(root.context("the workspace has no folder for a relative path; give an absolute one")?.join(path))
    }

    /// `path` as given, else the focused view's subject.
    fn subject_or(&self, path: Option<&str>) -> Result<PathBuf> {
        match path {
            Some(p) => self.resolve(p),
            None => self.focused_subject().context("no file is focused; give a path"),
        }
    }

    /// `dir` as given, else the folder of the focused subject (a folder
    /// itself, or a file's), else the workspace's directory.
    fn dir_or(&self, dir: Option<&str>) -> Result<PathBuf> {
        if let Some(d) = dir {
            return self.resolve(d);
        }
        let folder_of = |p: PathBuf| if p.is_dir() { Some(p) } else { p.parent().map(Path::to_path_buf) };
        let shown = self
            .focused_surface()
            .and_then(|sid| self.views.get(&sid))
            .and_then(|v| v.downcast_ref::<FileBrowserView>())
            .map(|b| b.tree().root().to_path_buf());
        self.focused_subject()
            .and_then(folder_of)
            .or(shown)
            .or_else(|| self.active().and_then(|vt| self.vtab_dir(vt)))
            .context("no folder to use; give a dir")
    }

    /// Tell every view that files changed (`moved`: each `(from, to)`), and
    /// keep the workspaces' pinned roots on the folders they were pinned to.
    pub(super) fn changed(&mut self, moved: &[(PathBuf, PathBuf)]) {
        let ids: Vec<_> = self.tree.vtabs().iter().map(|v| v.id).collect();
        for id in ids {
            if let Some(vtab) = self.tree.vtab_mut(id) {
                if let Some(root) = vtab.root_dir.as_deref().and_then(|r| file_ops::follow(r, moved)) {
                    vtab.root_dir = Some(root);
                }
            }
        }
        for view in self.views.values_mut() {
            view.files_changed(moved);
        }
        if let Some(clip) = &mut self.file_clip {
            if let Some(path) = file_ops::follow(&clip.path, moved) {
                clip.path = path;
            }
        }
    }

    /// Have every view with a file check it against the disk
    /// ([`View::changed_on_disk`]). Returns whether any view changed.
    pub fn check_open_files(&mut self) -> bool {
        let mut any = false;
        for view in self.views.values_mut() {
            if let Some(path) = view.path() {
                any |= view.changed_on_disk(&path);
            }
        }
        any
    }

    /// Create the file or folder `name` in `dir`.
    pub fn new_entry(&mut self, dir: &Path, name: &str, folder: bool) -> Result<PathBuf> {
        let path = if folder { file_ops::create_dir(dir, name)? } else { file_ops::create_file(dir, name)? };
        self.changed(&[]);
        Ok(path)
    }

    /// Remember `path` for Paste; `cut` moves it there instead of copying.
    pub fn clip_file(&mut self, path: &Path, cut: bool) {
        self.file_clip = Some(FileClip { path: path.to_path_buf(), cut });
    }

    /// Paste what Cut or Copy picked into `dir`, on the file worker. A cut
    /// is spent at once, so a second Paste finds nothing to paste; it comes
    /// back if the move fails. Returns what the paste says.
    pub fn paste_into(&mut self, dir: &Path) -> Result<String> {
        let clip = self.file_clip.clone().context("nothing to paste: Cut or Copy a file first")?;
        if !clip.path.exists() {
            bail!("{} is gone", clip.path.display());
        }
        if clip.cut {
            self.file_clip = None;
        }
        self.run_file_job(FileJob::Paste { src: clip.path, dir: dir.to_path_buf(), cut: clip.cut })
    }

    /// Copy `path` next to itself, on the file worker. Returns what the
    /// duplicate says.
    pub fn duplicate_path(&mut self, path: &Path) -> Result<String> {
        self.run_file_job(FileJob::Duplicate { src: path.to_path_buf() })
    }

    /// Wait for file jobs this long before a command returns (tests use zero
    /// to see every job finish in the background).
    #[cfg(test)]
    pub(crate) fn with_file_job_wait(mut self, wait: Duration) -> Self {
        self.file_job_wait = wait;
        self
    }

    /// Give `job` to the file worker and wait up to [`INLINE_WAIT`] for it,
    /// applying it (and any job before it) if it finishes. Returns what the
    /// command says: the job's outcome, or that it is running.
    fn run_file_job(&mut self, job: FileJob) -> Result<String> {
        let started = job.started();
        let id = self.file_jobs.submit(job);
        let deadline = Instant::now() + self.file_job_wait;
        while let Some(done) = self.file_jobs.take_until(deadline) {
            if done.id == id {
                let command = done.job.command();
                let (result, warning) = self.apply_file_job(done);
                // The command's reply goes to stderr only when it fails; a
                // warning is reported as one that finished later is.
                if let (true, Ok(message)) = (warning, &result) {
                    self.file_reports.push(FileReport { command, result: Ok(message.clone()), warning });
                }
                return result;
            }
            self.report_file_job(done);
        }
        Ok(started)
    }

    /// Apply the file jobs that finished. Returns whether any did.
    pub fn poll_file_jobs(&mut self) -> bool {
        let mut any = false;
        while let Some(done) = self.file_jobs.try_take() {
            self.report_file_job(done);
            any = true;
        }
        any
    }

    /// Block until every file job has finished, and apply them. For callers
    /// with no event loop to wake (the agent channel, tests).
    pub fn wait_file_jobs(&mut self) {
        for done in self.file_jobs.wait_all() {
            self.report_file_job(done);
        }
    }

    /// Whether every file job has finished.
    pub fn file_jobs_idle(&self) -> bool {
        self.file_jobs.idle()
    }

    /// A handle on the unfinished file jobs, for a quit to wait on.
    pub fn file_jobs_pending(&self) -> file_jobs::Pending {
        self.file_jobs.pending()
    }

    /// The outcomes of file jobs that finished after their command returned.
    pub fn take_file_reports(&mut self) -> Vec<FileReport> {
        std::mem::take(&mut self.file_reports)
    }

    fn report_file_job(&mut self, done: Done) {
        let command = done.job.command();
        let asking = self.confirm.is_some();
        let (result, warning) = self.apply_file_job(done);
        // A question from a job that finished after its command returned
        // comes while the user may be typing elsewhere.
        if let (false, Some(c)) = (asking, &mut self.confirm) {
            c.late = Some(Instant::now());
        }
        let result = result.map_err(|e| format!("{e:#}"));
        self.file_reports.push(FileReport { command, result, warning });
    }

    /// Bring the views up to date with a finished job. Returns what its
    /// command says, and whether that is a warning.
    fn apply_file_job(&mut self, done: Done) -> (Result<String>, bool) {
        if let (FileJob::Paste { src, cut: true, .. }, Ok(Output::Moved(moved))) = (&done.job, &done.result) {
            let renames = self.move_renames(src, moved);
            self.changed(&renames);
            return (Ok(pasted_message(moved)), moved.source_left);
        }
        (self.apply_file_job_result(done), false)
    }

    /// What a move of `src` to `moved.dest` moved, as renames for
    /// [`Self::changed`]. A move that left part of the original behind
    /// renames only what is gone from it: a file left there (changed while it
    /// was copied) keeps its views, which would otherwise show the older copy.
    fn move_renames(&self, src: &Path, moved: &file_ops::Moved) -> Vec<(PathBuf, PathBuf)> {
        if moved.dest == src {
            return Vec::new();
        }
        let whole = vec![(src.to_path_buf(), moved.dest.clone())];
        if !moved.source_left {
            return whole;
        }
        let browser_roots = self
            .views
            .values()
            .filter_map(|v| v.downcast_ref::<FileBrowserView>())
            .map(|b| b.tree().root().to_path_buf());
        let mut followed: Vec<PathBuf> = self
            .views
            .values()
            .flat_map(|v| [v.path(), v.subject()])
            .flatten()
            .chain(browser_roots)
            .chain(self.tree.vtabs().iter().filter_map(|v| v.root_dir.clone()))
            .chain(self.file_clip.as_ref().map(|c| c.path.clone()))
            .filter(|p| p.starts_with(src) && p.symlink_metadata().is_err())
            .collect();
        followed.sort();
        followed.dedup();
        followed.into_iter().filter_map(|p| Some((p.clone(), file_ops::follow(&p, &whole)?))).collect()
    }

    fn apply_file_job_result(&mut self, done: Done) -> Result<String> {
        let busy = done.busy;
        match (done.job, done.result) {
            (FileJob::Paste { src, cut: true, .. }, Err(e)) => {
                // The cut wasn't spent; unless something else was cut or
                // copied since, it can be pasted again.
                self.file_clip.get_or_insert(FileClip { path: src, cut: true });
                Err(e)
            }
            (FileJob::Paste { .. }, Ok(Output::Copied(dest))) => {
                self.changed(&[]);
                Ok(pasted_message(&file_ops::Moved { dest, source_left: false }))
            }
            (FileJob::Duplicate { .. }, Ok(Output::Copied(dest))) => {
                self.changed(&[]);
                Ok(format!("duplicated to {}", dest.display()))
            }
            // A copy or move using the path while git looked may have changed
            // what git saw (or be reading from it): ask.
            (FileJob::DeleteCheck { path }, Ok(Output::Clean(clean))) => {
                self.delete_checked(&path, clean && !busy).map(|()| String::new())
            }
            (_, Err(e)) => Err(e),
            (job, Ok(output)) => bail!("{} produced {output:?}", job.command()),
        }
    }

    /// Rename `path` to `name`; views of it follow. Returns the new path.
    pub fn rename_path(&mut self, path: &Path, name: &str) -> Result<PathBuf> {
        let dest = file_ops::rename(path, name)?;
        if dest != path {
            self.changed(&[(path.to_path_buf(), dest.clone())]);
        }
        Ok(dest)
    }

    /// The directory relative paths are taken from: the active workspace's,
    /// else `path`'s repository, else the current directory.
    fn relative_root(&self, path: &Path) -> PathBuf {
        self.active()
            .and_then(|vt| self.vtab_dir(vt))
            .filter(|root| path.starts_with(root))
            .or_else(|| git::repo_root(path))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// Put `path` (relative to the workspace root when `relative`) on the
    /// clipboard. Returns the text.
    pub fn copy_path_text(&mut self, path: &Path, relative: bool) -> String {
        let text = if relative {
            file_ops::relative_to(path, &self.relative_root(path))
        } else {
            path.to_string_lossy().into_owned()
        };
        self.clipboard_out = Some(text.clone());
        text
    }

    /// Add `path` to its repository's `.gitignore` (or `.git/info/exclude`).
    /// Returns whether the file changed.
    pub fn ignore_path(&mut self, path: &Path, exclude: bool) -> Result<bool> {
        let changed = if exclude { file_ops::add_to_exclude(path)? } else { file_ops::add_to_gitignore(path)? };
        self.changed(&[]);
        Ok(changed)
    }

    /// Open a terminal in `dir` (a file's folder).
    pub fn open_terminal_at(&mut self, path: &Path) -> Result<()> {
        let dir = if path.is_dir() { path } else { path.parent().context("no folder")? };
        self.open_plugin_in_focused_at(terminal::ID, Some(dir.to_path_buf()))
    }

    /// Show `path` in a file browser of the active workspace (focusing it), or
    /// in a new one.
    pub fn show_in_browser(&mut self, path: &Path) -> Result<()> {
        let vt = self.active().context("no workspace")?;
        let found = self.tree.vtab(vt).and_then(|v| {
            v.panes().into_iter().find_map(|p| {
                let i = p.surfaces.iter().position(|s| self.views.get(&s.id).is_some_and(|v| v.plugin() == file_browser::ID))?;
                Some((p.id, i, p.surfaces[i].id))
            })
        });
        let sid = match found {
            Some((pane, index, sid)) => {
                if let Some(v) = self.tree.vtab_mut(vt) {
                    v.focused_pane = pane;
                    if let Some(p) = v.pane_mut(pane) {
                        p.active = index;
                    }
                }
                sid
            }
            None => {
                self.open_plugin_in_focused(file_browser::ID)?;
                self.focused_surface().context("the file browser didn't open")?
            }
        };
        let browser = self
            .views
            .get_mut(&sid)
            .and_then(|v| v.downcast_mut::<FileBrowserView>())
            .context("the file browser didn't open")?;
        browser.reveal(path);
        Ok(())
    }

    /// The editor surface showing `path`, the focused one if several do.
    fn editor_of(&self, path: &Path) -> Option<SurfaceId> {
        let shows = |sid: &SurfaceId| {
            self.views.get(sid).is_some_and(|v| v.plugin() == editor::ID && v.path().as_deref() == Some(path))
        };
        self.focused_surface().filter(shows).or_else(|| {
            self.tree
                .vtabs()
                .iter()
                .flat_map(|v| v.panes())
                .flat_map(|p| p.surfaces.iter().map(|s| s.id))
                .find(shows)
        })
    }

    /// Ask where to save a copy of the editor showing `path`.
    pub fn save_copy_of(&mut self, path: &Path) -> Result<()> {
        let sid = self.editor_of(path).with_context(|| format!("{} isn't open in an editor", path.display()))?;
        self.open_save_copy(sid);
        Ok(())
    }

    /// The surfaces whose views show `path` or something inside it.
    fn surfaces_under(&self, path: &Path) -> Vec<SurfaceId> {
        self.tree
            .vtabs()
            .iter()
            .flat_map(|v| v.panes())
            .flat_map(|p| p.surfaces.iter().map(|s| s.id))
            .filter(|sid| self.views.get(sid).and_then(|v| v.path()).is_some_and(|p| p.starts_with(path)))
            .collect()
    }

    /// Delete `path`: move it to the Trash and close the views of it. Asks
    /// first unless git holds a copy of everything there (tracked and
    /// unmodified); a view with unsaved changes asks the way closing does.
    /// The git check runs on the file worker. Returns what the command says.
    pub fn delete_path(&mut self, path: &Path) -> Result<String> {
        if path.symlink_metadata().is_err() {
            bail!("{} doesn't exist", path.display());
        }
        let unsaved = self.unsaved_under(path);
        if !unsaved.is_empty() {
            self.ask_unsaved_trash(path, &unsaved);
            return Ok(String::new());
        }
        self.run_file_job(FileJob::DeleteCheck { path: path.to_path_buf() })
    }

    /// Finish a Delete once git said whether it holds a copy of `path`
    /// (`clean`). What changed during the check counts: the entry must still
    /// be there, a view of it edited meanwhile asks about its unsaved
    /// changes, and a question opened meanwhile (a quit) isn't replaced.
    fn delete_checked(&mut self, path: &Path, clean: bool) -> Result<()> {
        if path.symlink_metadata().is_err() {
            bail!("{} doesn't exist", path.display());
        }
        if self.confirm.is_some() {
            bail!("didn't delete {}: another question is open", path.display());
        }
        let unsaved = self.unsaved_under(path);
        if !unsaved.is_empty() {
            self.ask_unsaved_trash(path, &unsaved);
        } else if !clean {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            self.confirm = Some(Confirm {
                message: format!("Move \u{201c}{name}\u{201d} to the Trash?"),
                accept: Some("Delete"),
                discard: None,
                then: Confirmed::Trash(path.to_path_buf()),
                late: None,
            });
        } else {
            self.trash_now(path)?;
        }
        Ok(())
    }

    /// The surfaces showing `path` or something in it with unsaved changes
    /// (none when closing doesn't ask about them).
    fn unsaved_under(&self, path: &Path) -> Vec<SurfaceId> {
        if !self.guard_unsaved {
            return Vec::new();
        }
        self.surfaces_under(path).into_iter().filter(|s| self.views.get(s).is_some_and(|v| v.modified())).collect()
    }

    /// Ask whether to save `unsaved` (views of `path`) before trashing it.
    fn ask_unsaved_trash(&mut self, path: &Path, unsaved: &[SurfaceId]) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let (message, save, can_save) = self.unsaved_question(unsaved);
        self.confirm = Some(Confirm {
            message: format!("{message} Move \u{201c}{name}\u{201d} to the Trash?"),
            accept: can_save.then_some("Save and Delete"),
            discard: Some("Delete Without Saving"),
            then: Confirmed::UnsavedTrash { path: path.to_path_buf(), save },
            late: None,
        });
    }

    /// Move `path` to the Trash and close the views of it.
    pub(super) fn trash_now(&mut self, path: &Path) -> Result<()> {
        let open = self.surfaces_under(path);
        self.system.trash(path).with_context(|| format!("can't move {} to the Trash", path.display()))?;
        for sid in open {
            let place = self.tree.vtabs().iter().find_map(|v| {
                v.panes().into_iter().find(|p| p.surfaces.iter().any(|s| s.id == sid)).map(|p| (v.id, p.id))
            });
            if let Some((vt, pane)) = place {
                self.prune_orphan_surfaces_after(|s| s.tree.close_surface(vt, pane, sid));
            }
        }
        self.changed(&[]);
        Ok(())
    }
}

fn failed(e: anyhow::Error) -> CmdError {
    CmdError::Failed(format!("{e:#}"))
}

fn path_arg() -> ArgSpec {
    ArgSpec::optional("path", ArgKind::Str, "the file or folder (the focused one unless given)")
}

/// The result of a paste. A move across filesystems leaves in place what was
/// added to the original, or changed, while it was copied.
fn pasted_message(pasted: &file_ops::Moved) -> String {
    let left = if pasted.source_left { "; the original wasn't fully removed: what was added to it or changed during the move remains" } else { "" };
    format!("pasted {}{left}", pasted.dest.display())
}

fn dir_arg() -> ArgSpec {
    ArgSpec::optional("dir", ArgKind::Str, "the folder (the focused file's folder unless given)")
}

/// Register the `file.*` commands.
pub(super) fn register(r: &mut Registry<AppState>) {
    let mut path_command = |action: FileAction, description: &'static str, run: fn(&mut AppState, &Path) -> Result<String>| {
        r.register(
            CommandMeta::new(action.command(), action.title(), description).arg(path_arg()),
            Box::new(move |s: &mut AppState, a| {
                let path = s.subject_or(a.opt_str("path")).map_err(failed)?;
                let msg = run(s, &path).map_err(failed)?;
                Ok(if msg.is_empty() { CmdOutcome::ok() } else { CmdOutcome::msg(msg) })
            }),
        );
    };
    path_command(FileAction::Reveal, "Show a file or folder in the system file manager", |s, p| {
        s.system.reveal(p).map(|()| String::new())
    });
    path_command(FileAction::OpenDefault, "Open a file or folder with its default application", |s, p| {
        s.system.open_default(p).map(|()| String::new())
    });
    path_command(FileAction::OpenInTerminal, "Open a new terminal in a folder (a file's folder)", |s, p| {
        s.open_terminal_at(p).map(|()| String::new())
    });
    path_command(FileAction::ShowInBrowser, "Show a file in a file browser", |s, p| {
        s.show_in_browser(p).map(|()| String::new())
    });
    path_command(FileAction::Cut, "Cut a file or folder, to move it with Paste", |s, p| {
        s.clip_file(p, true);
        Ok(String::new())
    });
    path_command(FileAction::Copy, "Copy a file or folder, to copy it with Paste", |s, p| {
        s.clip_file(p, false);
        Ok(String::new())
    });
    path_command(FileAction::Duplicate, "Copy a file or folder next to itself", |s, p| s.duplicate_path(p));
    path_command(FileAction::CopyPath, "Copy a file's absolute path to the clipboard", |s, p| {
        Ok(s.copy_path_text(p, false))
    });
    path_command(FileAction::CopyRelativePath, "Copy a file's path relative to the workspace root", |s, p| {
        Ok(s.copy_path_text(p, true))
    });
    path_command(FileAction::AddToGitignore, "Add a file or folder to its repository's .gitignore", |s, p| {
        s.ignore_path(p, false).map(|changed| if changed { "added".into() } else { "already ignored".into() })
    });
    path_command(FileAction::AddToExclude, "Add a file or folder to its repository's .git/info/exclude", |s, p| {
        s.ignore_path(p, true).map(|changed| if changed { "added".into() } else { "already excluded".into() })
    });
    path_command(FileAction::SaveCopyAs, "Save a copy of an open editor's file to a file chosen in a picker", |s, p| {
        s.save_copy_of(p).map(|()| String::new())
    });
    path_command(
        FileAction::Delete,
        "Move a file or folder to the Trash, asking first unless git holds a copy; closes its editors",
        |s, p| s.delete_path(p),
    );

    for (action, folder, description) in [
        (FileAction::NewFile, false, "Create an empty file in a folder"),
        (FileAction::NewFolder, true, "Create a folder in a folder"),
    ] {
        r.register(
            CommandMeta::new(action.command(), action.title(), description)
                .arg(ArgSpec::required("name", ArgKind::Str, "the new name"))
                .arg(dir_arg()),
            Box::new(move |s: &mut AppState, a| {
                let dir = s.dir_or(a.opt_str("dir")).map_err(failed)?;
                let path = s.new_entry(&dir, a.get_str("name")?, folder).map_err(failed)?;
                Ok(CmdOutcome::msg(format!("created {}", path.display())))
            }),
        );
    }
    r.register(
        CommandMeta::new(
            FileAction::Paste.command(),
            FileAction::Paste.title(),
            "Paste the cut or copied file or folder into a folder",
        )
        .arg(dir_arg()),
        Box::new(|s: &mut AppState, a| {
            let dir = s.dir_or(a.opt_str("dir")).map_err(failed)?;
            Ok(CmdOutcome::msg(s.paste_into(&dir).map_err(failed)?))
        }),
    );
    r.register(
        CommandMeta::new(FileAction::Rename.command(), FileAction::Rename.title(), "Rename a file or folder")
            .arg(ArgSpec::required("name", ArgKind::Str, "the new name"))
            .arg(path_arg()),
        Box::new(|s: &mut AppState, a| {
            let path = s.subject_or(a.opt_str("path")).map_err(failed)?;
            let path = s.rename_path(&path, a.get_str("name")?).map_err(failed)?;
            Ok(CmdOutcome::msg(format!("renamed to {}", path.display())))
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ghostrealm_core::{Args, Value};

    use std::time::{Duration, Instant};

    use super::*;
    use crate::app_state::{build_registry, Pick};
    use crate::file_ops::tests::FakeSystem;
    use crate::file_menu::{Kind, Origin};
    use crate::plugin::testing::tmpdir;
    use crate::plugin::View;
    use crate::plugins::editor::EditorView;

    type Calls = Rc<RefCell<Vec<String>>>;

    fn app_with(fail_trash: bool) -> (AppState, Calls) {
        let calls = Calls::default();
        let system = FakeSystem { calls: calls.clone(), fail_trash };
        let mut s = AppState::new().with_shell_line("sleep 1").with_system(Box::new(system));
        s.new_empty_vtab();
        (s, calls)
    }

    fn app() -> (AppState, Calls) {
        app_with(false)
    }

    /// Run command `id` and wait for its file job, as the agent channel does:
    /// a job's failure is the command's.
    fn run(s: &mut AppState, id: &str, args: &[(&str, &Path)]) -> Result<CmdOutcome, CmdError> {
        let out = start(s, id, args)?;
        s.wait_file_jobs();
        match s.take_file_reports().into_iter().map(|r| r.result).collect::<Result<Vec<_>, _>>() {
            Ok(messages) if messages.is_empty() => Ok(out),
            Ok(messages) => Ok(messages.into_iter().rfind(|m| !m.is_empty()).map_or_else(CmdOutcome::ok, CmdOutcome::msg)),
            Err(e) => Err(CmdError::Failed(e)),
        }
    }

    /// Run command `id`, leaving its file job running.
    fn start(s: &mut AppState, id: &str, args: &[(&str, &Path)]) -> Result<CmdOutcome, CmdError> {
        let mut a = Args::new();
        for (k, v) in args {
            a.insert(k, Value::Str(v.to_string_lossy().into_owned()));
        }
        build_registry().execute(id, &a, s)
    }

    fn open_editor(s: &mut AppState, path: &Path) -> SurfaceId {
        s.open_path_with_in_focused(editor::ID, path).unwrap();
        s.focused_surface().unwrap()
    }

    fn editor_path(s: &AppState, sid: SurfaceId) -> Option<PathBuf> {
        s.view(sid).and_then(|v| v.downcast_ref::<EditorView>()).and_then(|e| e.buffer().path.clone())
    }

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().is_ok_and(|o| o.status.success())
    }

    /// A repository holding `tracked.txt`, committed; `None` without git.
    fn repo(tag: &str) -> Option<PathBuf> {
        let d = tmpdir(tag);
        if !git_ok(&d, &["init", "-q"]) {
            return None;
        }
        std::fs::write(d.join("tracked.txt"), "t\n").unwrap();
        assert!(git_ok(&d, &["add", "-A"]));
        assert!(git_ok(
            &d,
            &["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "c"]
        ));
        Some(d)
    }

    #[test]
    fn every_menu_action_is_a_registered_command() {
        let r = build_registry();
        for a in [
            FileAction::NewFile, FileAction::NewFolder, FileAction::Reveal, FileAction::ShowInBrowser,
            FileAction::OpenDefault, FileAction::OpenInTerminal, FileAction::Cut, FileAction::Copy,
            FileAction::Paste, FileAction::Duplicate, FileAction::CopyPath, FileAction::CopyRelativePath,
            FileAction::AddToGitignore, FileAction::AddToExclude, FileAction::Rename, FileAction::SaveCopyAs,
            FileAction::Delete,
        ] {
            assert!(r.contains(a.command()), "{}", a.command());
        }
        assert_eq!(ghostrealm_core::Config::default().binding("cmd+shift+c").as_deref(), Some("file.copy_path"));
    }

    #[test]
    fn the_menu_offers_paste_once_something_is_cut_or_copied_and_ignores_only_in_git() {
        let (mut s, _) = app();
        let d = tmpdir("fm-menu");
        std::fs::write(d.join("a.txt"), "a").unwrap();
        let target = FileTarget { path: d.join("a.txt"), kind: Kind::File, origin: Origin::Browser };
        let has = |s: &AppState, a| s.file_menu(&target).contains(&MenuEntry::Item(a));
        assert!(!has(&s, FileAction::Paste));
        run(&mut s, "file.copy", &[("path", &d.join("a.txt"))]).unwrap();
        assert!(has(&s, FileAction::Paste));
        if git::repo_root(&d).is_none() {
            assert!(!has(&s, FileAction::AddToGitignore), "not in a repository");
        }
        if let Some(r) = repo("fm-menu-git") {
            let t = FileTarget { path: r.join("tracked.txt"), ..target.clone() };
            assert!(s.file_menu(&t).contains(&MenuEntry::Item(FileAction::AddToGitignore)));
            // The repository's own folder is an entry of whatever holds it.
            if git::repo_root(r.parent().unwrap()).is_none() {
                let t = FileTarget { path: r.clone(), kind: Kind::Dir, ..target };
                assert!(!s.file_menu(&t).contains(&MenuEntry::Item(FileAction::AddToGitignore)));
            }
        }
    }

    #[test]
    fn new_files_and_folders_are_created_in_the_given_folder() {
        let (mut s, _) = app();
        let d = tmpdir("fm-new");
        run(&mut s, "file.new_file", &[("dir", &d), ("name", Path::new("a.txt"))]).unwrap();
        run(&mut s, "file.new_folder", &[("dir", &d), ("name", Path::new("sub"))]).unwrap();
        assert!(d.join("a.txt").is_file() && d.join("sub").is_dir());
        assert!(run(&mut s, "file.new_file", &[("dir", &d), ("name", Path::new("a.txt"))]).is_err(), "exists");
        assert!(run(&mut s, "file.new_file", &[("dir", &d), ("name", Path::new("x/y"))]).is_err(), "nested name");
        // A menu target maps to the command's arguments: a file's folder, a folder itself.
        let file = FileTarget { path: d.join("a.txt"), kind: Kind::File, origin: Origin::Editor };
        assert_eq!(FileAction::Paste.args(&file).get_str("dir").unwrap(), d.to_str().unwrap());
        assert_eq!(FileAction::Delete.args(&file).get_str("path").unwrap(), d.join("a.txt").to_str().unwrap());
    }

    #[test]
    fn new_entries_default_to_the_focused_files_folder() {
        let (mut s, _) = app();
        let d = tmpdir("fm-new-default");
        std::fs::write(d.join("a.txt"), "a").unwrap();
        open_editor(&mut s, &d.join("a.txt"));
        let args = Args::new().with("name", Value::Str("b.txt".into()));
        build_registry().execute("file.new_file", &args, &mut s).unwrap();
        assert!(d.join("b.txt").is_file());
    }

    #[test]
    fn duplicate_copy_and_cut_paste_work_on_the_filesystem() {
        let (mut s, _) = app();
        let d = tmpdir("fm-paste");
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("a.txt"), "A").unwrap();
        run(&mut s, "file.duplicate", &[("path", &d.join("a.txt"))]).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a copy.txt")).unwrap(), "A");
        // Copy then paste twice: the clipboard keeps the file.
        run(&mut s, "file.copy", &[("path", &d.join("a.txt"))]).unwrap();
        run(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        run(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        assert!(d.join("to/a.txt").is_file() && d.join("to/a copy.txt").is_file() && d.join("a.txt").is_file());
        // Cut then paste moves it, once.
        run(&mut s, "file.cut", &[("path", &d.join("a copy.txt"))]).unwrap();
        run(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        assert!(!d.join("a copy.txt").exists());
        assert!(d.join("to/a copy 2.txt").is_file(), "a taken name gets a suffix");
        assert!(run(&mut s, "file.paste", &[("dir", &d.join("to"))]).is_err(), "the cut was spent");
        assert!(s.file_clip().is_none());
    }

    #[test]
    fn pasting_with_nothing_cut_or_copied_says_so() {
        let (mut s, _) = app();
        let d = tmpdir("fm-paste-empty");
        let err = run(&mut s, "file.paste", &[("dir", &d)]).unwrap_err();
        assert!(err.to_string().contains("nothing to paste"), "{err}");
    }

    #[test]
    fn renaming_an_open_file_repoints_its_editor_and_a_folder_its_contents() {
        let (mut s, _) = app();
        let d = tmpdir("fm-rename");
        std::fs::create_dir(d.join("dir")).unwrap();
        std::fs::write(d.join("a.txt"), "A").unwrap();
        std::fs::write(d.join("dir/in.txt"), "I").unwrap();
        let a = open_editor(&mut s, &d.join("a.txt"));
        let inner = open_editor(&mut s, &d.join("dir/in.txt"));
        run(&mut s, "file.rename", &[("path", &d.join("a.txt")), ("name", Path::new("b.txt"))]).unwrap();
        assert!(d.join("b.txt").is_file() && !d.join("a.txt").exists());
        assert_eq!(editor_path(&s, a), Some(d.join("b.txt")));
        s.pump_all();
        assert_eq!(s.tree.vtabs()[0].panes()[0].surfaces[0].title, "b.txt", "the tab follows");
        run(&mut s, "file.rename", &[("path", &d.join("dir")), ("name", Path::new("renamed"))]).unwrap();
        assert_eq!(editor_path(&s, inner), Some(d.join("renamed/in.txt")));
        // A taken name changes nothing.
        assert!(run(&mut s, "file.rename", &[("path", &d.join("b.txt")), ("name", Path::new("renamed"))]).is_err());
        assert_eq!(editor_path(&s, a), Some(d.join("b.txt")));
        // The renamed editor saves to the new place.
        let v = s.view_mut(a).and_then(|v| v.downcast_mut::<EditorView>()).unwrap();
        v.buffer_mut().insert_str("x");
        assert!(v.buffer_mut().save().unwrap());
        assert!(std::fs::read_to_string(d.join("b.txt")).unwrap().starts_with('x'));
        assert!(!d.join("a.txt").exists());
    }

    #[test]
    fn moving_the_pinned_root_or_a_folder_above_it_keeps_the_workspace_on_it() {
        let (mut s, _) = app();
        let d = tmpdir("fm-root-moves");
        std::fs::create_dir_all(d.join("outer/proj")).unwrap();
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("outer/proj/f.txt"), "").unwrap();
        s.set_active_root_dir(d.join("outer/proj"));
        run(&mut s, "file.show_in_browser", &[("path", &d.join("outer/proj/f.txt"))]).unwrap();
        let browser = s.focused_surface().unwrap();
        let vt = s.active().unwrap();
        let other = s.new_workspace_in(d.join("outer"));
        s.tree.focus_vtab(vt);
        let root = |s: &AppState, vt| s.tree.vtab(vt).unwrap().root_dir.clone();
        // The root itself.
        run(&mut s, "file.rename", &[("path", &d.join("outer/proj")), ("name", Path::new("renamed"))]).unwrap();
        assert_eq!(root(&s, vt), Some(d.join("outer/renamed")));
        assert_eq!(s.vtab_dir(vt), Some(d.join("outer/renamed")));
        assert_eq!(root(&s, other), Some(d.join("outer")), "another workspace's root is untouched");
        // A folder above it, by a cut-paste move; every workspace under it follows.
        run(&mut s, "file.cut", &[("path", &d.join("outer"))]).unwrap();
        run(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        assert_eq!(root(&s, vt), Some(d.join("to/outer/renamed")));
        assert_eq!(root(&s, other), Some(d.join("to/outer")));
        let view = s.view(browser).and_then(|v| v.downcast_ref::<FileBrowserView>()).unwrap();
        assert!(view.tree().root().starts_with(d.join("to/outer")), "the browser's root follows too");
        // A new terminal starts in the moved folder.
        assert_eq!(s.resolve_cwd(vt), Some(d.join("to/outer/renamed")));
    }

    #[test]
    fn cut_paste_of_an_open_file_repoints_its_editor() {
        let (mut s, _) = app();
        let d = tmpdir("fm-cut-open");
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("a.txt"), "A").unwrap();
        let a = open_editor(&mut s, &d.join("a.txt"));
        run(&mut s, "file.cut", &[("path", &d.join("a.txt"))]).unwrap();
        run(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        assert_eq!(editor_path(&s, a), Some(d.join("to/a.txt")));
    }

    #[test]
    fn copy_path_puts_the_absolute_or_workspace_relative_path_on_the_clipboard() {
        let (mut s, _) = app();
        let d = tmpdir("fm-copy-path");
        std::fs::create_dir(d.join("src")).unwrap();
        std::fs::write(d.join("src/a.rs"), "").unwrap();
        s.set_active_root_dir(&d);
        let file = d.join("src/a.rs");
        run(&mut s, "file.copy_path", &[("path", &file)]).unwrap();
        assert_eq!(s.take_clipboard_text().as_deref(), file.to_str());
        assert_eq!(s.take_clipboard_text(), None, "taken once");
        run(&mut s, "file.copy_relative_path", &[("path", &file)]).unwrap();
        assert_eq!(s.take_clipboard_text().as_deref(), Some("src/a.rs"));
        // With no path, the focused editor's file.
        open_editor(&mut s, &file);
        build_registry().execute("file.copy_path", &Args::new(), &mut s).unwrap();
        assert_eq!(s.take_clipboard_text().as_deref(), file.to_str());
    }

    #[test]
    fn copy_path_with_nothing_focused_fails_clearly() {
        let (mut s, _) = app();
        let err = build_registry().execute("file.copy_path", &Args::new(), &mut s).unwrap_err();
        assert!(err.to_string().contains("no file is focused"), "{err}");
    }

    #[test]
    fn ignore_commands_append_to_gitignore_and_the_exclude_file_without_duplicating() {
        let Some(r) = repo("fm-ignore") else { return };
        let (mut s, _) = app();
        std::fs::create_dir(r.join("out")).unwrap();
        std::fs::write(r.join("out/x"), "").unwrap();
        for _ in 0..2 {
            run(&mut s, "file.add_to_gitignore", &[("path", &r.join("out"))]).unwrap();
            run(&mut s, "file.add_to_git_exclude", &[("path", &r.join("tracked.txt"))]).unwrap();
        }
        assert_eq!(std::fs::read_to_string(r.join(".gitignore")).unwrap(), "/out/\n");
        let exclude = std::fs::read_to_string(r.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == "/tracked.txt").count(), 1);
        let loose = tmpdir("fm-ignore-loose");
        std::fs::write(loose.join("f"), "").unwrap();
        if git::repo_root(&loose).is_none() {
            assert!(run(&mut s, "file.add_to_gitignore", &[("path", &loose.join("f"))]).is_err());
        }
    }

    #[test]
    fn reveal_and_open_default_go_through_the_system() {
        let (mut s, calls) = app();
        let d = tmpdir("fm-system");
        std::fs::write(d.join("a.txt"), "").unwrap();
        run(&mut s, "file.reveal", &[("path", &d.join("a.txt"))]).unwrap();
        run(&mut s, "file.open_default", &[("path", &d.join("a.txt"))]).unwrap();
        assert_eq!(
            *calls.borrow(),
            [format!("reveal {}", d.join("a.txt").display()), format!("open {}", d.join("a.txt").display())]
        );
    }

    #[test]
    fn open_in_terminal_starts_a_terminal_in_the_folder() {
        let mut s = AppState::new().with_shell_line("pwd; sleep 2");
        s.new_empty_vtab();
        let d = tmpdir("fm-terminal").join("here-dir");
        std::fs::create_dir_all(&d).unwrap();
        run(&mut s, "file.open_in_terminal", &[("path", &d)]).unwrap();
        let sid = s.focused_surface().unwrap();
        assert_eq!(s.view(sid).map(|v| v.plugin()), Some(terminal::ID));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut text = String::new();
        while Instant::now() < deadline && !text.contains("here-dir") {
            text = s.surface_text(sid).unwrap_or_default();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(text.contains("here-dir"), "the shell started in the folder; got:\n{text}");
        // A file means its folder.
        let f = d.join("f.txt");
        std::fs::write(&f, "").unwrap();
        run(&mut s, "file.open_in_terminal", &[("path", &f)]).unwrap();
        assert_ne!(s.focused_surface(), Some(sid), "another terminal");
    }

    #[test]
    fn show_in_file_browser_opens_one_on_the_file_then_reuses_it() {
        let (mut s, _) = app();
        let d = tmpdir("fm-show");
        std::fs::create_dir_all(d.join("deep/er")).unwrap();
        std::fs::write(d.join("deep/er/f.txt"), "").unwrap();
        s.set_active_root_dir(&d);
        let editor = open_editor(&mut s, &d.join("deep/er/f.txt"));
        run(&mut s, "file.show_in_browser", &[("path", &d.join("deep/er/f.txt"))]).unwrap();
        let browser = s.focused_surface().unwrap();
        assert_ne!(browser, editor);
        let view = s.view(browser).and_then(|v| v.downcast_ref::<FileBrowserView>()).unwrap();
        assert_eq!(view.subject(), Some(d.join("deep/er/f.txt")), "selected");
        assert!(view.tree().is_expanded(&d.join("deep")) && view.tree().is_expanded(&d.join("deep/er")));
        // Focus elsewhere, ask again: the same browser comes back, no new tab.
        let vt = s.active().unwrap();
        let pane = s.tree.vtab(vt).unwrap().focused_pane;
        s.tree.vtab_mut(vt).unwrap().pane_mut(pane).unwrap().active = 0;
        let before = s.tree.surface_count();
        run(&mut s, "file.show_in_browser", &[("path", &d.join("deep/er/f.txt"))]).unwrap();
        assert_eq!(s.tree.surface_count(), before);
        assert_eq!(s.focused_surface(), Some(browser));
    }

    #[test]
    fn save_copy_as_writes_a_copy_and_leaves_the_buffer_alone() {
        let (mut s, _) = app();
        let d = tmpdir("fm-copy-as");
        std::fs::write(d.join("a.txt"), "A\n").unwrap();
        let sid = open_editor(&mut s, &d.join("a.txt"));
        s.view_mut(sid).and_then(|v| v.downcast_mut::<EditorView>()).unwrap().buffer_mut().insert_str("edit ");
        run(&mut s, "file.save_copy_as", &[("path", &d.join("a.txt"))]).unwrap();
        assert_eq!(s.picker_purpose(), Some(Pick::SaveCopy(sid)));
        s.confirm_picker_with(d.join("sub/copy.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("sub/copy.txt")).unwrap(), "edit A\n", "the unsaved text");
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "A\n", "the original is untouched");
        assert_eq!(editor_path(&s, sid), Some(d.join("a.txt")), "not re-pointed");
        assert!(s.view(sid).unwrap().modified(), "still unsaved");
        // Copying onto an existing file asks first, even onto the buffer's own.
        run(&mut s, "file.save_copy_as", &[("path", &d.join("a.txt"))]).unwrap();
        assert_eq!(s.confirm_picker_with(d.join("a.txt")).unwrap(), None);
        assert!(s.confirm().is_some());
        // Only an open editor can be copied.
        assert!(run(&mut s, "file.save_copy_as", &[("path", &d.join("nope.txt"))]).is_err());
    }

    #[test]
    fn deleting_an_untracked_file_confirms_then_trashes_it_and_closes_its_editor() {
        let (mut s, calls) = app();
        let d = tmpdir("fm-delete");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        open_editor(&mut s, &d.join("a.txt"));
        run(&mut s, "file.delete", &[("path", &d.join("a.txt"))]).unwrap();
        let c = s.confirm().expect("not in git: ask first");
        assert_eq!(c.accept, Some("Delete"));
        assert!(c.message.contains("a.txt"));
        assert!(d.join("a.txt").exists() && calls.borrow().is_empty(), "nothing yet");
        s.cancel_confirm();
        assert!(d.join("a.txt").exists());
        run(&mut s, "file.delete", &[("path", &d.join("a.txt"))]).unwrap();
        s.accept_confirm().unwrap();
        assert_eq!(*calls.borrow(), [format!("trash {}", d.join("a.txt").display())]);
        assert!(!d.join("a.txt").exists());
        assert_eq!(s.views().count(), 0, "the editor closed like Cmd+W");
        assert!(s.confirm().is_none());
    }

    #[test]
    fn deleting_a_tracked_unmodified_file_needs_no_confirmation() {
        let Some(r) = repo("fm-delete-git") else { return };
        let (mut s, calls) = app();
        open_editor(&mut s, &r.join("tracked.txt"));
        run(&mut s, "file.delete", &[("path", &r.join("tracked.txt"))]).unwrap();
        assert!(s.confirm().is_none());
        assert_eq!(*calls.borrow(), [format!("trash {}", r.join("tracked.txt").display())]);
        assert_eq!(s.views().count(), 0);
    }

    #[test]
    fn deleting_a_tracked_file_with_changes_or_a_folder_with_untracked_files_confirms() {
        let Some(r) = repo("fm-delete-dirty") else { return };
        let (mut s, calls) = app();
        std::fs::write(r.join("tracked.txt"), "changed\n").unwrap();
        run(&mut s, "file.delete", &[("path", &r.join("tracked.txt"))]).unwrap();
        assert!(s.confirm().is_some(), "modified");
        s.cancel_confirm();
        std::fs::write(r.join("tracked.txt"), "t\n").unwrap();
        std::fs::create_dir(r.join("sub")).unwrap();
        std::fs::write(r.join("sub/new.txt"), "n").unwrap();
        run(&mut s, "file.delete", &[("path", &r.join("sub"))]).unwrap();
        assert!(s.confirm().is_some(), "untracked");
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn deleting_a_folder_closes_the_editors_inside_it() {
        let (mut s, _) = app();
        let d = tmpdir("fm-delete-dir");
        std::fs::create_dir(d.join("dir")).unwrap();
        std::fs::write(d.join("dir/a.txt"), "A").unwrap();
        std::fs::write(d.join("keep.txt"), "K").unwrap();
        open_editor(&mut s, &d.join("dir/a.txt"));
        open_editor(&mut s, &d.join("keep.txt"));
        run(&mut s, "file.delete", &[("path", &d.join("dir"))]).unwrap();
        s.accept_confirm().unwrap();
        assert!(!d.join("dir").exists());
        let open: Vec<_> = s.views().filter_map(|(_, v)| v.path()).collect();
        assert_eq!(open, [d.join("keep.txt")]);
    }

    #[test]
    fn checking_open_files_reloads_an_editor_whose_file_another_program_wrote() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let d = tmpdir("fm-check-open");
        std::fs::write(d.join("a.txt"), "A\n").unwrap();
        let sid = open_editor(&mut s, &d.join("a.txt"));
        assert!(!s.check_open_files());
        std::fs::write(d.join("a.txt"), "Bee\n").unwrap();
        assert!(s.check_open_files());
        let lines = &s.view(sid).and_then(|v| v.downcast_ref::<EditorView>()).unwrap().buffer().lines;
        assert_eq!(lines, &["Bee"]);
        assert!(!s.check_open_files(), "once");
    }

    #[test]
    fn deleting_a_file_with_unsaved_changes_asks_the_unsaved_question() {
        let calls = Calls::default();
        let mut s = AppState::new()
            .with_unsaved_guard()
            .with_shell_line("sleep 1")
            .with_system(Box::new(FakeSystem { calls: calls.clone(), fail_trash: false }));
        s.new_empty_vtab();
        let d = tmpdir("fm-delete-unsaved");
        std::fs::write(d.join("a.txt"), "A\n").unwrap();
        let sid = open_editor(&mut s, &d.join("a.txt"));
        s.view_mut(sid).and_then(|v| v.downcast_mut::<EditorView>()).unwrap().buffer_mut().insert_str("x");
        run(&mut s, "file.delete", &[("path", &d.join("a.txt"))]).unwrap();
        let c = s.confirm().unwrap();
        assert_eq!((c.accept, c.discard), (Some("Save and Delete"), Some("Delete Without Saving")));
        assert!(c.message.contains("unsaved changes"));
        s.cancel_confirm();
        assert!(calls.borrow().is_empty() && s.view(sid).is_some(), "Cancel changes nothing");
        run(&mut s, "file.delete", &[("path", &d.join("a.txt"))]).unwrap();
        s.discard_confirm().unwrap();
        assert_eq!(*calls.borrow(), [format!("trash {}", d.join("a.txt").display())]);
        assert!(s.view(sid).is_none(), "closed without saving");
    }

    #[test]
    fn a_failing_trash_keeps_the_file_and_its_editor() {
        let (mut s, calls) = app_with(true);
        let d = tmpdir("fm-delete-fail");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        let sid = open_editor(&mut s, &d.join("a.txt"));
        run(&mut s, "file.delete", &[("path", &d.join("a.txt"))]).unwrap();
        let err = s.accept_confirm().unwrap_err();
        assert!(format!("{err:#}").contains("no Trash here"), "{err:#}");
        assert!(d.join("a.txt").exists() && s.view(sid).is_some() && calls.borrow().is_empty());
    }

    #[test]
    fn deleting_something_missing_fails_without_asking() {
        let (mut s, _) = app();
        let d = tmpdir("fm-delete-missing");
        assert!(run(&mut s, "file.delete", &[("path", &d.join("nope"))]).is_err());
        assert!(s.confirm().is_none());
    }

    #[test]
    fn relative_paths_are_taken_from_the_workspace_folder_not_the_apps() {
        let (mut s, _) = app();
        let d = tmpdir("fm-relative");
        std::fs::write(d.join("x.txt"), "x\n").unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();
        assert!(run(&mut s, "file.rename", &[("path", Path::new("x.txt")), ("name", Path::new("y.txt"))]).is_err());
        s.set_active_root_dir(&d);
        let sid = open_editor(&mut s, &d.join("x.txt"));
        run(&mut s, "file.rename", &[("path", Path::new("x.txt")), ("name", Path::new("y.txt"))]).unwrap();
        assert!(d.join("y.txt").exists());
        assert_eq!(editor_path(&s, sid), Some(d.join("y.txt")), "the editor follows");
        run(&mut s, "file.new_file", &[("dir", Path::new("sub")), ("name", Path::new("n.txt"))]).unwrap();
        assert!(d.join("sub/n.txt").exists());
    }

    /// An app whose file commands never wait for their job: each finishes
    /// in the background and is applied by a pump.
    fn app_in_background() -> (AppState, Calls) {
        let calls = Calls::default();
        let system = FakeSystem { calls: calls.clone(), fail_trash: false };
        let mut s = AppState::new()
            .with_shell_line("sleep 1")
            .with_system(Box::new(system))
            .with_file_job_wait(Duration::ZERO);
        s.new_empty_vtab();
        (s, calls)
    }

    /// Pump `s` as the event loop does until a file job's outcome arrives.
    fn pump_until_reported(s: &mut AppState) -> Vec<FileReport> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            s.pump_all();
            let reports = s.take_file_reports();
            if !reports.is_empty() {
                return reports;
            }
            assert!(Instant::now() < deadline, "no file job finished");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn browser_text(s: &mut AppState, sid: SurfaceId) -> String {
        s.view_mut(sid).unwrap().text().unwrap()
    }

    #[test]
    fn a_big_paste_finishes_in_the_background_and_the_browser_then_shows_it() {
        let (mut s, _) = app_in_background();
        let d = tmpdir("fm-bg-paste");
        crate::file_jobs::tests::big_tree(&d.join("src"), 200);
        std::fs::create_dir(d.join("to")).unwrap();
        s.set_active_root_dir(d.join("to"));
        s.open_plugin_in_focused(file_browser::ID).unwrap();
        let browser = s.focused_surface().unwrap();
        assert!(!browser_text(&mut s, browser).contains("src"));
        run(&mut s, "file.copy", &[("path", &d.join("src"))]).unwrap();
        let out = start(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        assert_eq!(out.message, Some(format!("pasting into {}", d.join("to").display())));
        assert!(!browser_text(&mut s, browser).contains("src"), "nothing is applied before a pump");
        let reports = pump_until_reported(&mut s);
        assert_eq!(
            reports,
            [FileReport {
                command: "file.paste",
                result: Ok(format!("pasted {}", d.join("to/src").display())),
                warning: false,
            }]
        );
        assert!(browser_text(&mut s, browser).contains("src"), "the listing re-read once it finished");
        let copied = walkdir_count(&d.join("to/src"));
        assert_eq!(copied, 200);
    }

    fn walkdir_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .map(|p| if p.is_dir() { walkdir_count(&p) } else { 1 })
            .sum()
    }

    #[test]
    fn the_delete_check_runs_in_the_background_and_decides_as_before() {
        let Some(r) = repo("fm-bg-delete") else { return };
        let (mut s, calls) = app_in_background();
        // Tracked and unmodified: trashed without asking, once the check is in.
        start(&mut s, "file.delete", &[("path", &r.join("tracked.txt"))]).unwrap();
        assert!(s.confirm().is_none() && calls.borrow().is_empty(), "nothing before the check is in");
        assert_eq!(pump_until_reported(&mut s)[0].result, Ok(String::new()));
        assert!(s.confirm().is_none());
        assert_eq!(*calls.borrow(), [format!("trash {}", r.join("tracked.txt").display())]);
        // Untracked: asks, with the same question.
        std::fs::write(r.join("new.txt"), "n").unwrap();
        start(&mut s, "file.delete", &[("path", &r.join("new.txt"))]).unwrap();
        assert!(s.confirm().is_none());
        pump_until_reported(&mut s);
        let c = s.confirm().expect("untracked: ask first");
        assert_eq!((c.accept, c.discard), (Some("Delete"), None));
        assert!(c.message.contains("new.txt"));
        assert_eq!(calls.borrow().len(), 1, "nothing more trashed");
    }

    #[test]
    fn a_question_a_late_job_asks_ignores_keys_for_a_moment() {
        let (mut s, _) = app_in_background();
        let d = tmpdir("fm-bg-late-confirm");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        start(&mut s, "file.delete", &[("path", &d.join("a.txt"))]).unwrap();
        pump_until_reported(&mut s);
        let now = Instant::now();
        let c = s.confirm().expect("untracked: asks");
        assert!(!c.takes_keys_at(now), "an Enter typed meanwhile doesn't answer it");
        assert!(c.takes_keys_at(now + crate::app_state::LATE_CONFIRM_GRACE));
        assert!(c.enter_cancels(), "even later, Enter doesn't delete");
        s.enter_confirm().unwrap();
        assert!(s.confirm().is_none() && d.join("a.txt").exists(), "cancelled");
        // One the user's own action opens takes keys at once.
        let mut s = AppState::new().with_unsaved_guard().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let sid = open_editor(&mut s, &d.join("a.txt"));
        s.view_mut(sid).and_then(|v| v.downcast_mut::<EditorView>()).unwrap().buffer_mut().insert_str("x");
        run(&mut s, "file.delete", &[("path", &d.join("a.txt"))]).unwrap();
        assert!(s.confirm().unwrap().takes_keys_at(Instant::now()));
        assert!(!s.confirm().unwrap().enter_cancels(), "Enter answers it as before");
    }

    fn guarded_app() -> AppState {
        let mut s = AppState::new().with_unsaved_guard().with_shell_line("sleep 1");
        s.new_empty_vtab();
        s
    }

    #[test]
    fn quitting_while_a_copy_runs_asks_to_quit_anyway_or_cancel() {
        let d = tmpdir("fm-quit-busy");
        // Quit Anyway: at once.
        let mut s = guarded_app();
        s.file_jobs.submit(FileJob::Hold { src: d.join("a"), dir: d.join("b"), until: d.join("go") });
        s.request_quit();
        let c = s.confirm().expect("a copy runs: ask");
        assert_eq!((c.accept, c.discard), (Some("Quit Anyway"), None));
        assert!(c.enter_cancels(), "a stray Enter doesn't cut the copy off");
        assert!(!s.quitting());
        s.accept_confirm().unwrap();
        assert!(s.quitting());
        std::fs::write(d.join("go"), "").unwrap();
        // Cancel: the app stays, the copy goes on, and a later quit asks again.
        let mut s = guarded_app();
        s.file_jobs.submit(FileJob::Hold { src: d.join("a"), dir: d.join("b"), until: d.join("go2") });
        s.request_quit();
        assert_eq!(s.enter_confirm().unwrap(), Vec::<PathBuf>::new());
        assert!(s.confirm().is_none() && !s.quitting(), "Enter cancelled");
        s.request_quit();
        assert!(s.confirm().is_some(), "asks again");
        s.cancel_confirm();
        std::fs::write(d.join("go2"), "").unwrap();
        s.wait_file_jobs();
        s.request_quit();
        assert!(s.confirm().is_none() && s.quitting(), "nothing running: quits");
    }

    #[test]
    fn a_quit_asks_about_unsaved_changes_then_about_a_running_copy() {
        let d = tmpdir("fm-quit-unsaved-busy");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        let mut s = guarded_app();
        let sid = open_editor(&mut s, &d.join("a.txt"));
        s.view_mut(sid).and_then(|v| v.downcast_mut::<EditorView>()).unwrap().buffer_mut().insert_str("x");
        s.file_jobs.submit(FileJob::Hold { src: d.join("a"), dir: d.join("b"), until: d.join("go") });
        s.request_quit();
        assert_eq!(s.confirm().unwrap().discard, Some("Quit Without Saving"));
        s.discard_confirm().unwrap();
        assert_eq!(s.confirm().unwrap().accept, Some("Quit Anyway"), "then the copy");
        assert!(!s.quitting());
        std::fs::write(d.join("go"), "").unwrap();
    }

    #[test]
    fn deleting_what_a_running_copy_reads_from_or_writes_into_asks_first() {
        let Some(r) = repo("fm-delete-busy") else { return };
        std::fs::create_dir(r.join("to")).unwrap();
        std::fs::write(r.join("to/kept.txt"), "k\n").unwrap();
        assert!(git_ok(&r, &["add", "-A"]));
        assert!(git_ok(&r, &["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "to"]));
        for (target, why) in [(r.join("tracked.txt"), "the copy's source"), (r.join("to"), "the folder pasted into")] {
            let (mut s, calls) = app();
            let go = tmpdir("fm-delete-busy-go").join("go");
            s.file_jobs.submit(FileJob::Hold { src: r.join("tracked.txt"), dir: r.join("to"), until: go.clone() });
            run_until_check(&mut s, &target);
            assert!(s.confirm().is_some(), "{why}: tracked and clean, but in use, so ask");
            assert!(calls.borrow().is_empty());
            std::fs::write(&go, "").unwrap();
            s.wait_file_jobs();
        }
        // Once the copy is done, a clean file goes without asking again.
        let (mut s, calls) = app();
        run_until_check(&mut s, &r.join("tracked.txt"));
        assert!(s.confirm().is_none());
        assert_eq!(calls.borrow().len(), 1);
    }

    /// Start a Delete of `path` and apply its check (only: a held copy may
    /// still run).
    fn run_until_check(s: &mut AppState, path: &Path) {
        start(s, "file.delete", &[("path", path)]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while s.file_jobs.checking() {
            assert!(Instant::now() < deadline, "the check never finished");
            s.poll_file_jobs();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_running_delete_check_doesnt_hold_up_a_quit() {
        let d = tmpdir("fm-quit-check");
        let mut s = guarded_app();
        s.file_jobs.submit(FileJob::DeleteCheck { path: d });
        s.request_quit();
        assert!(s.confirm().is_none() && s.quitting());
    }

    #[test]
    fn a_delete_check_that_finishes_under_another_question_deletes_nothing() {
        let (mut s, calls) = app_in_background();
        let d = tmpdir("fm-bg-delete-asked");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        std::fs::write(d.join("b.txt"), "B").unwrap();
        start(&mut s, "file.delete", &[("path", &d.join("a.txt"))]).unwrap();
        start(&mut s, "file.delete", &[("path", &d.join("b.txt"))]).unwrap();
        s.wait_file_jobs();
        let reports = s.take_file_reports();
        assert_eq!(reports[0].result, Ok(String::new()));
        let err = reports[1].result.as_ref().unwrap_err();
        assert!(err.contains("another question is open"), "{err}");
        assert!(s.confirm().unwrap().message.contains("a.txt"), "the first question stays");
        assert!(d.join("b.txt").exists() && calls.borrow().is_empty());
    }

    #[test]
    fn a_cut_is_spent_at_once_and_comes_back_if_the_move_fails() {
        let (mut s, _) = app_in_background();
        let d = tmpdir("fm-bg-cut");
        std::fs::create_dir_all(d.join("dir/sub")).unwrap();
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("a.txt"), "A").unwrap();
        run(&mut s, "file.cut", &[("path", &d.join("a.txt"))]).unwrap();
        start(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        assert!(s.file_clip().is_none());
        let err = start(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap_err();
        assert!(err.to_string().contains("nothing to paste"), "{err}");
        s.wait_file_jobs();
        assert!(d.join("to/a.txt").is_file() && !d.join("a.txt").exists());
        // Into itself: refused on the worker, and the cut is back.
        run(&mut s, "file.cut", &[("path", &d.join("dir"))]).unwrap();
        assert!(run(&mut s, "file.paste", &[("dir", &d.join("dir/sub"))]).is_err());
        assert_eq!(s.file_clip(), Some(&FileClip { path: d.join("dir"), cut: true }));
    }

    #[test]
    fn closing_the_editor_of_a_file_being_moved_is_harmless() {
        let (mut s, _) = app_in_background();
        let d = tmpdir("fm-bg-close");
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("a.txt"), "A").unwrap();
        std::fs::write(d.join("b.txt"), "B").unwrap();
        let a = open_editor(&mut s, &d.join("a.txt"));
        let b = open_editor(&mut s, &d.join("b.txt"));
        run(&mut s, "file.cut", &[("path", &d.join("a.txt"))]).unwrap();
        start(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        run(&mut s, "file.cut", &[("path", &d.join("b.txt"))]).unwrap();
        start(&mut s, "file.paste", &[("dir", &d.join("to"))]).unwrap();
        s.close_surface_id(a);
        s.wait_file_jobs();
        assert!(d.join("to/a.txt").is_file() && d.join("to/b.txt").is_file());
        assert!(s.view(a).is_none());
        assert_eq!(editor_path(&s, b), Some(d.join("to/b.txt")), "the open one follows");
    }

    #[test]
    fn a_move_that_left_an_open_file_behind_keeps_its_editor_there_and_warns() {
        let (mut s, _) = app();
        let d = tmpdir("fm-move-left");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("to/src")).unwrap();
        for name in ["left.txt", "moved.txt"] {
            std::fs::write(d.join("src").join(name), "old").unwrap();
            std::fs::write(d.join("to/src").join(name), "old").unwrap();
        }
        let left = open_editor(&mut s, &d.join("src/left.txt"));
        let moved = open_editor(&mut s, &d.join("src/moved.txt"));
        // As a move across filesystems leaves it: the copy is in, the copied
        // file removed, and the one written during the copy left in place.
        std::fs::remove_file(d.join("src/moved.txt")).unwrap();
        std::fs::write(d.join("src/left.txt"), "new").unwrap();
        let done = Done {
            id: 0,
            job: FileJob::Paste { src: d.join("src"), dir: d.join("to"), cut: true },
            result: Ok(Output::Moved(file_ops::Moved { dest: d.join("to/src"), source_left: true })),
            busy: false,
        };
        s.report_file_job(done);
        assert_eq!(editor_path(&s, left), Some(d.join("src/left.txt")), "stays on the newer file");
        assert_eq!(editor_path(&s, moved), Some(d.join("to/src/moved.txt")), "follows the move");
        let reports = s.take_file_reports();
        assert!(reports[0].warning, "{reports:?}");
        assert!(reports[0].result.as_ref().unwrap().contains("wasn't fully removed"));
    }

    #[test]
    fn a_paste_that_left_part_of_the_original_says_what_and_why() {
        let whole = file_ops::Moved { dest: PathBuf::from("/to/a"), source_left: false };
        assert_eq!(pasted_message(&whole), "pasted /to/a");
        let left = file_ops::Moved { dest: PathBuf::from("/to/a"), source_left: true };
        let msg = pasted_message(&left);
        assert!(msg.starts_with("pasted /to/a; the original wasn't fully removed"), "{msg}");
        assert!(msg.contains("added") && msg.contains("changed"), "both reasons are named: {msg}");
    }
}
