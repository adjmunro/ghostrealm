//! The file commands' slow work, on a worker thread: copies (Paste,
//! Duplicate), moves (a cut's Paste, which across filesystems copies and then
//! removes), and Delete's git check. A big folder or a slow `git` would
//! otherwise freeze the window.
//!
//! One thread runs the copies and moves one at a time, in the order they were
//! given, so an operation always sees the result of the ones before it (a
//! second paste of the same file picks its free name after the first has
//! taken one). Delete's checks only read, and run on a second thread, so one
//! never waits behind a long copy. Each finished job comes back as a [`Done`]
//! for the UI thread to apply; the workers wake the UI when one is waiting.
//! Dropping [`FileJobs`] lets the workers finish what they were given; the
//! process exiting does not, so a quit asks while a copy or move is
//! [`Pending`].

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::Result;
use ghostrealm_core::git;

use crate::file_ops;
use crate::plugin::Waker;

/// A file operation to run on the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileJob {
    /// Copy (or, `cut`, move) `src` into `dir` under a free name.
    Paste { src: PathBuf, dir: PathBuf, cut: bool },
    /// Copy `src` next to itself.
    Duplicate { src: PathBuf },
    /// Find whether git holds a copy of everything at `path` (tracked and
    /// unmodified), which decides whether Delete asks first.
    DeleteCheck { path: PathBuf },
    /// A copy of `src` into `dir` that runs until `until` exists, for tests
    /// that need one in flight.
    #[cfg(test)]
    Hold { src: PathBuf, dir: PathBuf, until: PathBuf },
}

/// What a job produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    /// Where a copy went.
    Copied(PathBuf),
    /// Where a moved entry went.
    Moved(file_ops::Moved),
    /// Whether git holds a copy of everything at the path.
    Clean(bool),
}

impl FileJob {
    /// The command this job does the work of, for reports.
    pub fn command(&self) -> &'static str {
        match self {
            FileJob::Paste { .. } => "file.paste",
            FileJob::Duplicate { .. } => "file.duplicate",
            FileJob::DeleteCheck { .. } => "file.delete",
            #[cfg(test)]
            FileJob::Hold { .. } => "file.paste",
        }
    }

    /// What the command says while the job is still running.
    pub fn started(&self) -> String {
        match self {
            FileJob::Paste { dir, .. } => format!("pasting into {}", dir.display()),
            FileJob::Duplicate { src } => format!("duplicating {}", src.display()),
            FileJob::DeleteCheck { path } => format!("checking {} with git", path.display()),
            #[cfg(test)]
            FileJob::Hold { until, .. } => format!("holding until {}", until.display()),
        }
    }

    /// What a copy or move reads from and writes into: its source and the
    /// folder it copies into.
    fn uses(&self) -> Vec<PathBuf> {
        match self {
            FileJob::Paste { src, dir, .. } => vec![src.clone(), dir.clone()],
            FileJob::Duplicate { src } => src.parent().map(Path::to_path_buf).into_iter().chain([src.clone()]).collect(),
            FileJob::DeleteCheck { .. } => Vec::new(),
            #[cfg(test)]
            FileJob::Hold { src, dir, .. } => vec![src.clone(), dir.clone()],
        }
    }

    fn run(&self) -> Result<Output> {
        Ok(match self {
            FileJob::Paste { src, dir, cut: true } => Output::Moved(file_ops::move_into(src, dir)?),
            FileJob::Paste { src, dir, cut: false } => Output::Copied(file_ops::copy_into(src, dir)?),
            FileJob::Duplicate { src } => Output::Copied(file_ops::duplicate(src)?),
            FileJob::DeleteCheck { path } => Output::Clean(git::is_tracked_clean(path)),
            #[cfg(test)]
            FileJob::Hold { until, .. } => {
                while !until.exists() {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Output::Copied(until.clone())
            }
        })
    }
}

/// A finished job.
#[derive(Debug)]
pub struct Done {
    /// What [`FileJobs::submit`] returned for it.
    pub id: u64,
    pub job: FileJob,
    pub result: Result<Output>,
    /// For Delete's check: a copy or move used the path (or a folder around
    /// or inside it) while the check ran, so git's answer may be out of date.
    pub busy: bool,
}

/// The number of jobs given to the worker and not finished yet, shared with
/// whoever must wait for them (a quit).
#[derive(Clone, Default)]
pub struct Pending(Arc<(Mutex<usize>, Condvar)>);

impl Pending {
    fn count(&self) -> std::sync::MutexGuard<'_, usize> {
        self.0 .0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add(&self) {
        *self.count() += 1;
    }

    fn finish(&self) {
        *self.count() -= 1;
        self.0 .1.notify_all();
    }

    /// Whether every job given has finished.
    pub fn idle(&self) -> bool {
        *self.count() == 0
    }

    /// Block until every job given has finished.
    pub fn wait_idle(&self) {
        let mut count = self.count();
        while *count > 0 {
            count = self.0 .1.wait(count).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// The two lanes and the channel both send their results on.
struct Lanes {
    copies: Sender<(u64, FileJob)>,
    checks: Sender<(u64, FileJob)>,
    done: Receiver<Done>,
}

/// The file-operation workers: started on the first job.
#[derive(Default)]
pub struct FileJobs {
    lanes: Option<Lanes>,
    waker: Option<Waker>,
    /// Copies and moves not finished yet: what a quit must not cut off.
    pending: Pending,
    /// Delete's checks not finished yet. Only reads, so a quit drops them.
    checking: Pending,
    /// Copies and moves given and not taken back yet, with what they use.
    active: Vec<(u64, Vec<PathBuf>)>,
    /// Delete's checks not taken back yet: their path, and whether a copy or
    /// move used it since the check was given.
    checks: HashMap<u64, ([PathBuf; 2], bool)>,
    next_id: u64,
}

/// Whether `a` and `b` are the same entry or one holds the other (whole
/// path components: `/a/b` is not inside `/a/bc`).
fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// `path` as given and spelled one way, for [`overlap`]. Its final
/// component is kept unresolved, so a symlink entry stays itself (unless it
/// is `..` or `.`, which name a folder through the rest). The rest is
/// resolved through its longest existing ancestor, by the kernel (which
/// follows its symlinks and `..` as the filesystem does: `/tmp` is
/// `/private/tmp` on macOS), and what follows that, which doesn't exist, by
/// spelling. Both forms count: deleting a link to a folder still breaks a
/// copy reading through it. (`Path::components` already drops an interior
/// `.`; only `..` and a leading `.` are left to resolve.)
fn spellings(path: &Path) -> [PathBuf; 2] {
    let (prefix, last) = match path.components().next_back() {
        Some(Component::Normal(name)) => (path.parent().unwrap_or(Path::new("")), Some(name)),
        _ => (path, None),
    };
    let resolved = prefix
        .ancestors()
        .find_map(|dir| Some((dir.canonicalize().ok()?, prefix.strip_prefix(dir).ok()?)))
        .map_or_else(|| lexical(PathBuf::new(), prefix), |(dir, rest)| lexical(dir, rest));
    let resolved = match last {
        Some(name) => resolved.join(name),
        None => resolved,
    };
    [path.to_path_buf(), resolved]
}

/// `rest` applied to `base` by spelling alone: `.` dropped, and each `..`
/// taking the component before it (for components that don't exist, so the
/// disk can't say).
fn lexical(base: PathBuf, rest: &Path) -> PathBuf {
    let mut out = base;
    for c in rest.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir if matches!(out.components().next_back(), Some(Component::Normal(_))) => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

impl FileJobs {
    /// Wake the UI with `waker` when a job finishes. Applies to workers
    /// started after this call.
    pub fn set_waker(&mut self, waker: Option<Waker>) {
        self.waker = waker;
    }

    /// The id the next job [`submit`](Self::submit)ted gets. Ids rise in
    /// submission order.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Queue `job` behind the ones of its lane given before it: copies and
    /// moves on one, Delete's checks on another, so a check never waits
    /// behind a long copy. Returns its id.
    pub fn submit(&mut self, job: FileJob) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let check = matches!(job, FileJob::DeleteCheck { .. });
        if check { &self.checking } else { &self.pending }.add();
        if let FileJob::DeleteCheck { path } = &job {
            let path = spellings(path);
            let busy = self.active.iter().flat_map(|(_, uses)| uses).any(|u| path.iter().any(|p| overlap(u, p)));
            self.checks.insert(id, (path, busy));
        } else {
            let uses: Vec<PathBuf> = job.uses().iter().flat_map(|p| spellings(p)).collect();
            for (path, busy) in self.checks.values_mut() {
                *busy |= uses.iter().any(|u| path.iter().any(|p| overlap(u, p)));
            }
            self.active.push((id, uses));
        }
        let lanes = self.lanes.get_or_insert_with(|| {
            let (outbox, done) = mpsc::channel();
            Lanes {
                copies: spawn("file-jobs", outbox.clone(), self.waker.clone(), self.pending.clone()),
                checks: spawn("file-checks", outbox, self.waker.clone(), self.checking.clone()),
                done,
            }
        });
        // The threads catch every panic, so they run until this end drops.
        if check { &lanes.checks } else { &lanes.copies }.send((id, job)).expect("the file-jobs workers are running");
        id
    }

    /// A finished job, if one is waiting.
    pub fn try_take(&mut self) -> Option<Done> {
        let done = self.lanes.as_ref()?.done.try_recv().ok()?;
        Some(self.taken(done))
    }

    /// A finished job, waiting for one until `deadline`.
    pub fn take_until(&mut self, deadline: Instant) -> Option<Done> {
        let lanes = self.lanes.as_ref()?;
        let wait = deadline.saturating_duration_since(Instant::now());
        let done = lanes.done.recv_timeout(wait).ok()?;
        Some(self.taken(done))
    }

    /// Note that `done` was taken back: a copy or move no longer uses its
    /// paths, and a check learns whether one did meanwhile.
    fn taken(&mut self, mut done: Done) -> Done {
        self.active.retain(|(id, _)| *id != done.id);
        if let Some((_, busy)) = self.checks.remove(&done.id) {
            done.busy = busy;
        }
        done
    }

    /// Block until every job given has finished, and return them all, each
    /// lane's in order.
    pub fn wait_all(&mut self) -> Vec<Done> {
        self.pending.wait_idle();
        self.checking.wait_idle();
        std::iter::from_fn(|| self.try_take()).collect()
    }

    /// Block until every job given since id `mark` ([`Self::next_id`] then)
    /// has been taken back, and return them with any earlier job that
    /// finished meanwhile. Earlier jobs still running aren't waited for.
    pub fn wait_since(&mut self, mark: u64) -> Vec<Done> {
        let mut out = Vec::new();
        while self.active.iter().any(|(id, _)| *id >= mark) || self.checks.keys().any(|id| *id >= mark) {
            let Some(Ok(done)) = self.lanes.as_ref().map(|l| l.done.recv()) else { break };
            out.push(self.taken(done));
        }
        out
    }

    /// Whether every copy and move given has finished (its result may still
    /// be waiting). Delete's checks don't count: a quit drops them.
    pub fn idle(&self) -> bool {
        self.pending.idle()
    }

    /// Whether a Delete check hasn't been taken back yet.
    #[cfg(test)]
    pub fn checking(&self) -> bool {
        !self.checks.is_empty()
    }

    /// Whether a Delete check is still running.
    #[cfg(test)]
    pub fn checking_unfinished(&self) -> bool {
        !self.checking.idle()
    }

    /// A handle on the count of unfinished copies and moves.
    pub fn pending(&self) -> Pending {
        self.pending.clone()
    }
}

/// Start a lane: a thread that runs its jobs in order, sends each result on
/// `outbox`, and counts it off `pending`.
fn spawn(name: &str, outbox: Sender<Done>, waker: Option<Waker>, pending: Pending) -> Sender<(u64, FileJob)> {
    let (jobs, inbox) = mpsc::channel::<(u64, FileJob)>();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            while let Ok((id, job)) = inbox.recv() {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run()))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("the file operation failed unexpectedly")));
                // Sent before the count drops, so once idle every result is
                // waiting. A closed UI end still lets the queued jobs run.
                let _ = outbox.send(Done { id, job, result, busy: false });
                pending.finish();
                if let Some(wake) = &waker {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake()));
                }
            }
        })
        .expect("spawn a file-jobs worker");
    jobs
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::plugin::testing::tmpdir;
    use std::path::Path;
    use std::time::Duration;

    /// A folder of `files` files of 16 KiB each, spread over a few subfolders.
    pub fn big_tree(dir: &Path, files: usize) {
        let block = vec![b'x'; 16 * 1024];
        for i in 0..files {
            let sub = dir.join(format!("d{}", i % 8));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("f{i}.bin")), &block).unwrap();
        }
    }

    fn count_files(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .map(|p| if p.is_dir() { count_files(&p) } else { 1 })
            .sum()
    }

    #[test]
    fn a_big_copy_runs_on_the_worker_and_wakes_the_ui_when_done() {
        let d = tmpdir("jobs-big");
        big_tree(&d.join("src"), 200);
        let (tx, woken) = mpsc::channel();
        let tx = Mutex::new(tx);
        let mut jobs = FileJobs::default();
        jobs.set_waker(Some(Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        })));
        let id = jobs.submit(FileJob::Duplicate { src: d.join("src") });
        woken.recv_timeout(Duration::from_secs(30)).expect("woken when done");
        assert!(jobs.idle());
        let done = jobs.try_take().expect("the result is waiting");
        assert_eq!(done.id, id);
        assert_eq!(done.result.unwrap(), Output::Copied(d.join("src copy")));
        assert_eq!(count_files(&d.join("src copy")), 200);
        assert!(jobs.try_take().is_none());
    }

    #[test]
    fn jobs_run_in_the_order_given_and_each_sees_the_last() {
        let d = tmpdir("jobs-order");
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("a.txt"), "A").unwrap();
        let mut jobs = FileJobs::default();
        let paste = || FileJob::Paste { src: d.join("a.txt"), dir: d.join("to"), cut: false };
        let ids = [jobs.submit(paste()), jobs.submit(paste()), jobs.submit(paste())];
        let done = jobs.wait_all();
        assert_eq!(done.iter().map(|d| d.id).collect::<Vec<_>>(), ids);
        let dests: Vec<_> = done.into_iter().map(|d| d.result.unwrap()).collect();
        assert_eq!(
            dests,
            [
                Output::Copied(d.join("to/a.txt")),
                Output::Copied(d.join("to/a copy.txt")),
                Output::Copied(d.join("to/a copy 2.txt")),
            ]
        );
    }

    #[test]
    fn a_failing_job_reports_its_error_and_the_next_still_runs() {
        let d = tmpdir("jobs-fail");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        let mut jobs = FileJobs::default();
        jobs.submit(FileJob::Duplicate { src: d.join("missing") });
        jobs.submit(FileJob::Duplicate { src: d.join("a.txt") });
        let done = jobs.wait_all();
        assert!(done[0].result.is_err());
        assert_eq!(done[1].result.as_ref().unwrap(), &Output::Copied(d.join("a copy.txt")));
    }

    #[test]
    fn waiting_for_idle_blocks_until_the_running_copy_is_whole() {
        let d = tmpdir("jobs-idle");
        big_tree(&d.join("src"), 100);
        let mut jobs = FileJobs::default();
        jobs.submit(FileJob::Paste { src: d.join("src"), dir: d.join("src/d0"), cut: false });
        jobs.submit(FileJob::Duplicate { src: d.join("src") });
        let pending = jobs.pending();
        std::thread::spawn(move || pending.wait_idle()).join().unwrap();
        assert!(jobs.idle());
        assert_eq!(count_files(&d.join("src copy")), 100, "nothing cut off half done");
        let done = jobs.wait_all();
        assert!(done[0].result.is_err(), "not into itself");
    }

    #[test]
    fn a_delete_check_doesnt_wait_behind_a_copy_or_count_as_pending() {
        let d = tmpdir("jobs-lanes");
        let mut jobs = FileJobs::default();
        jobs.submit(FileJob::DeleteCheck { path: d.to_path_buf() });
        assert!(jobs.idle(), "a check alone holds up nothing");
        jobs.wait_all();
        let hold = jobs.submit(FileJob::Hold { src: d.join("a"), dir: d.join("b"), until: d.join("go") });
        let check = jobs.submit(FileJob::DeleteCheck { path: d.to_path_buf() });
        let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).expect("the check finishes first");
        assert_eq!(done.id, check);
        assert!(!jobs.idle(), "the copy still runs");
        std::fs::write(d.join("go"), "").unwrap();
        let done = jobs.wait_all();
        assert_eq!(done.iter().map(|d| d.id).collect::<Vec<_>>(), [hold]);
        assert!(jobs.idle());
    }

    #[test]
    fn a_check_learns_whether_a_copy_used_its_path_by_whole_components() {
        let d = tmpdir("jobs-busy");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        let mut jobs = FileJobs::default();
        let hold = |until: &str| FileJob::Hold { src: d.join("a/b"), dir: d.join("to"), until: d.join(until) };
        jobs.submit(hold("go"));
        for (path, busy) in [("a", true), ("a/b/c", true), ("a/bc", false), ("to/x", true), ("elsewhere", false)] {
            jobs.submit(FileJob::DeleteCheck { path: d.join(path) });
            let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).unwrap();
            assert_eq!(done.busy, busy, "{path}");
        }
        // Given after the check started: the check hears of it.
        jobs.submit(FileJob::Hold { src: d.join("x"), dir: d.join("y"), until: d.join("go") });
        std::fs::write(d.join("go"), "").unwrap();
        let done = jobs.wait_all();
        assert_eq!(done.len(), 2);
        let check = jobs.submit(FileJob::DeleteCheck { path: d.join("late") });
        jobs.submit(FileJob::Duplicate { src: d.join("late/f") });
        let done = jobs.wait_all();
        assert!(done.iter().find(|x| x.id == check).unwrap().busy);
    }

    #[cfg(unix)]
    #[test]
    fn a_check_sees_a_copy_through_a_symlinked_folder() {
        let d = tmpdir("jobs-busy-link");
        std::fs::create_dir_all(d.join("real/a")).unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
        let mut jobs = FileJobs::default();
        jobs.submit(FileJob::Hold { src: d.join("link/a"), dir: d.join("to"), until: d.join("go") });
        jobs.submit(FileJob::DeleteCheck { path: d.join("real/a") });
        let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).unwrap();
        assert!(done.busy, "the same folder, spelled through the link");
        // The link the copy reads through, though only a link.
        jobs.submit(FileJob::DeleteCheck { path: d.join("link") });
        let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).unwrap();
        assert!(done.busy, "it holds the copy's source as spelled");
        // Inside the destination, spelled through the link, before it exists.
        jobs.submit(FileJob::DeleteCheck { path: d.join("link/../to/new") });
        jobs.submit(FileJob::DeleteCheck { path: d.join("elsewhere") });
        let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).unwrap();
        assert!(done.busy, "inside the destination folder");
        let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).unwrap();
        assert!(!done.busy);
        std::fs::write(d.join("go"), "").unwrap();
        jobs.wait_all();
    }

    #[test]
    fn a_dotted_path_matches_the_folder_it_names() {
        let d = tmpdir("jobs-busy-dots");
        std::fs::create_dir_all(d.join("real/a")).unwrap();
        std::fs::create_dir_all(d.join("real/b")).unwrap();
        let mut jobs = FileJobs::default();
        jobs.submit(FileJob::Hold { src: d.join("real/b"), dir: d.join("to"), until: d.join("go") });
        for (path, busy) in [("real/a/..", true), ("real/./b", true), ("real/a/../a", false), ("nope/../to/x", true)] {
            jobs.submit(FileJob::DeleteCheck { path: d.join(path) });
            let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).unwrap();
            assert_eq!(done.busy, busy, "{path}");
        }
        std::fs::write(d.join("go"), "").unwrap();
        jobs.wait_all();
    }

    #[cfg(unix)]
    #[test]
    fn a_dotted_path_resolves_as_the_kernel_does_and_keeps_a_final_link() {
        let d = tmpdir("jobs-busy-dots-links");
        std::fs::create_dir_all(d.join("x")).unwrap();
        std::fs::create_dir_all(d.join("other/real")).unwrap();
        std::fs::create_dir_all(d.join("other/to")).unwrap();
        std::os::unix::fs::symlink(d.join("other/real"), d.join("link")).unwrap();
        let mut jobs = FileJobs::default();
        // A copy of the link itself; one into `link/..`'s `to` (the kernel's
        // `other/to`, not the spelling's `to`).
        jobs.submit(FileJob::Hold { src: d.join("link"), dir: d.join("elsewhere"), until: d.join("go") });
        jobs.submit(FileJob::Hold { src: d.join("src"), dir: d.join("other/to"), until: d.join("go") });
        for (path, busy) in [("x/../link", true), ("link/../to/nonex", true), ("to/nonex", false)] {
            jobs.submit(FileJob::DeleteCheck { path: d.join(path) });
            let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).unwrap();
            assert_eq!(done.busy, busy, "{path}");
        }
        std::fs::write(d.join("go"), "").unwrap();
        jobs.wait_all();
    }

    #[test]
    fn take_until_gives_up_at_the_deadline() {
        let mut jobs = FileJobs::default();
        assert!(jobs.take_until(Instant::now() + Duration::from_millis(5)).is_none(), "no worker yet");
        let d = tmpdir("jobs-deadline");
        jobs.submit(FileJob::DeleteCheck { path: d.to_path_buf() });
        let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).expect("arrives");
        assert_eq!(done.job, FileJob::DeleteCheck { path: d.to_path_buf() });
        assert!(matches!(done.result.unwrap(), Output::Clean(_)));
    }
}
