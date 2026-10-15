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

use std::path::PathBuf;
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
    /// A copy that runs until `until` exists, for tests that need one in
    /// flight.
    #[cfg(test)]
    Hold { until: PathBuf },
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
            FileJob::Hold { until } => format!("holding until {}", until.display()),
        }
    }

    fn run(&self) -> Result<Output> {
        Ok(match self {
            FileJob::Paste { src, dir, cut: true } => Output::Moved(file_ops::move_into(src, dir)?),
            FileJob::Paste { src, dir, cut: false } => Output::Copied(file_ops::copy_into(src, dir)?),
            FileJob::Duplicate { src } => Output::Copied(file_ops::duplicate(src)?),
            FileJob::DeleteCheck { path } => Output::Clean(git::is_tracked_clean(path)),
            #[cfg(test)]
            FileJob::Hold { until } => {
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
    next_id: u64,
}

impl FileJobs {
    /// Wake the UI with `waker` when a job finishes. Applies to workers
    /// started after this call.
    pub fn set_waker(&mut self, waker: Option<Waker>) {
        self.waker = waker;
    }

    /// Queue `job` behind the ones of its lane given before it: copies and
    /// moves on one, Delete's checks on another, so a check never waits
    /// behind a long copy. Returns its id.
    pub fn submit(&mut self, job: FileJob) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let check = matches!(job, FileJob::DeleteCheck { .. });
        if check { &self.checking } else { &self.pending }.add();
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
        self.lanes.as_ref()?.done.try_recv().ok()
    }

    /// A finished job, waiting for one until `deadline`.
    pub fn take_until(&mut self, deadline: Instant) -> Option<Done> {
        let lanes = self.lanes.as_ref()?;
        let wait = deadline.saturating_duration_since(Instant::now());
        lanes.done.recv_timeout(wait).ok()
    }

    /// Block until every job given has finished, and return them all, each
    /// lane's in order.
    pub fn wait_all(&mut self) -> Vec<Done> {
        self.pending.wait_idle();
        self.checking.wait_idle();
        std::iter::from_fn(|| self.try_take()).collect()
    }

    /// Whether every copy and move given has finished (its result may still
    /// be waiting). Delete's checks don't count: a quit drops them.
    pub fn idle(&self) -> bool {
        self.pending.idle()
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
                let _ = outbox.send(Done { id, job, result });
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
        jobs.submit(FileJob::DeleteCheck { path: d.clone() });
        assert!(jobs.idle(), "a check alone holds up nothing");
        jobs.wait_all();
        let hold = jobs.submit(FileJob::Hold { until: d.join("go") });
        let check = jobs.submit(FileJob::DeleteCheck { path: d.clone() });
        let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).expect("the check finishes first");
        assert_eq!(done.id, check);
        assert!(!jobs.idle(), "the copy still runs");
        std::fs::write(d.join("go"), "").unwrap();
        let done = jobs.wait_all();
        assert_eq!(done.iter().map(|d| d.id).collect::<Vec<_>>(), [hold]);
        assert!(jobs.idle());
    }

    #[test]
    fn take_until_gives_up_at_the_deadline() {
        let mut jobs = FileJobs::default();
        assert!(jobs.take_until(Instant::now() + Duration::from_millis(5)).is_none(), "no worker yet");
        let d = tmpdir("jobs-deadline");
        jobs.submit(FileJob::DeleteCheck { path: d.clone() });
        let done = jobs.take_until(Instant::now() + Duration::from_secs(30)).expect("arrives");
        assert_eq!(done.job, FileJob::DeleteCheck { path: d });
        assert!(matches!(done.result.unwrap(), Output::Clean(_)));
    }
}
