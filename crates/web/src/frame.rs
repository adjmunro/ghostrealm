//! Page pixels cross the process boundary through a memory-mapped file rather
//! than the JSON pipe (a full HiDPI frame is tens of megabytes).
//!
//! Each browser owns one file holding two BGRA slots of equal size. The host
//! writes alternate slots and announces each with [`Event::Frame`], so the app
//! reads one slot while the host fills the other. A file is never shrunk or
//! truncated while mapped: when a larger frame no longer fits, the host creates a
//! new file under a fresh name and unlinks the old one (existing mappings stay
//! valid until the reader remaps).
//!
//! The app can fall two frames behind, so the host may be rewriting the slot it
//! reads. Each slot has a sequence word in the file's header, used as a seqlock:
//! the writer zeroes it, writes the pixels, then stores the frame's sequence
//! number; the reader copies the pixels and keeps them only if the word held
//! that number before and after the copy.
//!
//! [`Event::Frame`]: crate::protocol::Event::Frame

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{fence, AtomicU64, Ordering};

/// Bytes before the first slot: a sequence word per slot, padded.
const HEADER: usize = 64;

/// Frame files are `<PREFIX><host pid>-<page id>-<generation>.bgra`.
const PREFIX: &str = "ghostrealm-web-";

/// The directory frame files go in: `$XDG_RUNTIME_DIR/ghostrealm` when that is
/// set, else `<temp dir>/ghostrealm-<uid>`. It is created mode 0700 and refused
/// unless it is a directory this user owns that nobody else can access: in a
/// shared temp dir another user could otherwise read frames or take their
/// predictable names first. When that directory is refused or can't be made
/// (another user pre-created it, a stale `XDG_RUNTIME_DIR` inherited through
/// `su`), a fresh randomly named private directory in the temp dir is used;
/// [`remove_stale_dirs`] clears those after their host is gone.
pub fn frame_dir() -> io::Result<PathBuf> {
    frame_dir_from(&std::env::temp_dir())
}

/// [`frame_dir`] with `temp` as the temp dir: the host reads it before it
/// points `TMPDIR` at Chromium's own (`crate::temp`).
pub fn frame_dir_from(temp: &Path) -> io::Result<PathBuf> {
    let preferred = match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        Some(run) => run.join("ghostrealm"),
        None => temp.join(format!("ghostrealm-{}", user_id())),
    };
    frame_dir_in(&preferred, temp)
}

/// `preferred` if it is usable as a private directory, else a fresh private
/// directory under `fallback_base`.
fn frame_dir_in(preferred: &Path, fallback_base: &Path) -> io::Result<PathBuf> {
    match private_dir(preferred) {
        Ok(()) => Ok(preferred.to_path_buf()),
        Err(_) => random_private_dir(fallback_base),
    }
}

/// Create a new directory (mode 0700) named `ghostrealm-<uid>-<pid>-<random>`
/// in `base`, as `mkdtemp` does: creation fails if the name exists, so the
/// directory is ours and was never planted.
fn random_private_dir(base: &Path) -> io::Result<PathBuf> {
    use std::hash::{BuildHasher, Hasher};
    let mut last = None;
    for _ in 0..16 {
        // RandomState is seeded from the OS per instance.
        let n = std::collections::hash_map::RandomState::new().build_hasher().finish();
        let dir = base.join(format!("ghostrealm-{}-{}-{n:016x}", user_id(), std::process::id()));
        match create_private(&dir) {
            Ok(()) => {
                private_dir(&dir)?;
                return Ok(dir);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free directory name")))
}

#[cfg(unix)]
fn create_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private(dir: &Path) -> io::Result<()> {
    std::fs::create_dir(dir)
}

#[cfg(unix)]
pub(crate) fn user_id() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub(crate) fn user_id() -> u32 {
    0
}

/// Create `dir` (mode 0700) if it is missing, and check that it is a real
/// directory (not a symlink) owned by this user with no access for others.
#[cfg(unix)]
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(dir) {
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() || meta.uid() != user_id() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory private to this user", dir.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Remove frame files in `dir` left by hosts that are no longer `running`
/// (a host that crashed can't remove its own).
pub fn remove_stale(dir: &Path, running: impl Fn(u32) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.ends_with(".bgra"))
            .and_then(|n| n.strip_prefix(PREFIX))
            .and_then(|n| n.split('-').next())
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if !running(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Clear the fallback frame directories ([`frame_dir`]) that earlier hosts left
/// in `base`: for each `ghostrealm-<uid>-<pid>-<random>` directory this user
/// owns, private (mode 0700), whose host is no longer `running`, remove its
/// frame files and then the directory. A running host's directory is not
/// touched, nor is anything not matching that name and ownership.
pub fn remove_stale_dirs(base: &Path, running: impl Fn(u32) -> bool) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let uid = user_id().to_string();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("ghostrealm-")) else {
            continue;
        };
        let parts: Vec<&str> = rest.split('-').collect();
        let [owner, pid, _random] = parts[..] else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let dir = entry.path();
        if owner != uid || running(pid) || private_dir_exists(&dir).is_err() {
            continue;
        }
        remove_stale(&dir, |_| false);
        // Fails, and so keeps the directory, if anything else is in it.
        let _ = std::fs::remove_dir(&dir);
    }
}

/// Whether `dir` exists as a real directory private to this user, without
/// creating it.
#[cfg(unix)]
fn private_dir_exists(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_dir() && meta.uid() == user_id() && meta.mode() & 0o777 == 0o700 {
        Ok(())
    } else {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
}

#[cfg(not(unix))]
fn private_dir_exists(dir: &Path) -> io::Result<()> {
    std::fs::symlink_metadata(dir).map(|_| ())
}

/// Bytes in a `width x height` BGRA frame, or `None` if that overflows (the
/// sizes come from the other process).
fn frame_bytes(width: u32, height: u32) -> Option<usize> {
    (width as usize).checked_mul(height as usize)?.checked_mul(4)
}

/// Slot `slot`'s sequence word in a mapping starting at `base`.
///
/// # Safety
/// `base` must point to a mapping at least [`HEADER`] bytes long, page-aligned
/// (so the word is aligned), that outlives the returned reference.
unsafe fn seq_word<'a>(base: *const u8, slot: u8) -> &'a AtomicU64 {
    // SAFETY: per the caller; slot is 0 or 1, so the word lies in the header.
    unsafe { &*(base.add(slot as usize * 8) as *const AtomicU64) }
}

use memmap2::{Mmap, MmapMut};

/// Host side: owns the current frame file for one browser.
pub struct FrameWriter {
    dir: PathBuf,
    prefix: String,
    generation: u64,
    current: Option<(PathBuf, MmapMut)>,
    seq: u64,
}

/// Where a written frame landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenFrame {
    pub path: String,
    pub slot: u8,
    pub seq: u64,
}

impl FrameWriter {
    /// The writer for page `id` of this host process, creating its files in
    /// `dir`.
    pub fn for_page(dir: impl Into<PathBuf>, id: u64) -> Self {
        Self::new(dir, format!("{PREFIX}{}-{id}", std::process::id()))
    }

    /// Frame files are created in `dir` as `<prefix>-<generation>.bgra`.
    pub fn new(dir: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        FrameWriter {
            dir: dir.into(),
            prefix: prefix.into(),
            generation: 0,
            current: None,
            seq: 0,
        }
    }

    /// Copy one BGRA frame (`width * height * 4` bytes, tightly packed) into the
    /// next slot.
    pub fn write(&mut self, width: u32, height: u32, pixels: &[u8]) -> io::Result<WrittenFrame> {
        let Some(bytes) = frame_bytes(width, height).filter(|b| pixels.len() >= *b) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "short frame buffer",
            ));
        };
        let fits = self
            .current
            .as_ref()
            .is_some_and(|(_, map)| (map.len() - HEADER) / 2 >= bytes);
        if !fits {
            self.grow(bytes)?;
        }
        let (path, map) = self.current.as_mut().expect("frame file allocated");
        self.seq += 1;
        let slot = (self.seq % 2) as u8;
        let slot_bytes = (map.len() - HEADER) / 2;
        let start = HEADER + slot as usize * slot_bytes;
        // One raw pointer for both the word and the pixels: a `&mut [u8]` over
        // the mapping would invalidate the atomic derived from it.
        let base = map.as_mut_ptr();
        // SAFETY: the mapping is page-aligned and longer than the header.
        let word = unsafe { seq_word(base, slot) };
        word.store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        // SAFETY: `start + bytes` is within the slot (it fits), and `pixels`
        // is a separate allocation.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), base.add(start), bytes) };
        word.store(self.seq, Ordering::Release);
        Ok(WrittenFrame {
            path: path.to_string_lossy().into_owned(),
            slot,
            seq: self.seq,
        })
    }

    fn grow(&mut self, bytes: usize) -> io::Result<()> {
        self.generation += 1;
        let path = self
            .dir
            .join(format!("{}-{}.bgra", self.prefix, self.generation));
        let open = || {
            let mut o = OpenOptions::new();
            o.read(true).write(true).create_new(true);
            // Pages can show private content: only this user may read frames.
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            o.open(&path)
        };
        // A new file, never one already there (nor through a symlink planted
        // under the name). A leftover from an earlier process with this pid is
        // removed once.
        let file = match open() {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(&path)?;
                open()?
            }
            r => r?,
        };
        // The name is ours now: any failure below must not leave the file
        // behind, as the next frame would make another one.
        let mapped = (|| {
            let len = bytes
                .checked_mul(2)
                .and_then(|b| b.checked_add(HEADER))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
            reserve(&file, len as u64)?;
            // SAFETY: the file is private to this writer (fresh, unique name)
            // and is never truncated while mapped.
            unsafe { MmapMut::map_mut(&file) }
        })();
        let map = match mapped {
            Ok(map) => map,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };
        if let Some((old, _)) = self.current.replace((path, map)) {
            let _ = std::fs::remove_file(old);
        }
        Ok(())
    }
}

/// Size `file` to `len` bytes with its blocks allocated where the platform can
/// (`posix_fallocate`), so a full tmpfs fails here with `ENOSPC` instead of
/// raising SIGBUS when pixels are later written through the mapping.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn reserve(file: &std::fs::File, len: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is open for the duration of the call.
    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len as libc::off_t) };
    match rc {
        0 => Ok(()),
        // The filesystem can't preallocate: fall back to a sparse file.
        libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS => file.set_len(len),
        e => Err(io::Error::from_raw_os_error(e)),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
fn reserve(file: &std::fs::File, len: u64) -> io::Result<()> {
    file.set_len(len)
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        if let Some((path, _)) = self.current.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// App side: a read-only mapping of one frame file.
pub struct FrameReader {
    path: String,
    map: Mmap,
}

impl FrameReader {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = std::fs::File::open(Path::new(path))?;
        // SAFETY: the writer never truncates a mapped file; it only replaces it.
        let map = unsafe { Mmap::map(&file)? };
        Ok(FrameReader {
            path: path.to_string(),
            map,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Where `slot`'s pixels for a `width x height` frame lie, if the file
    /// holds them.
    fn span(&self, slot: u8, width: u32, height: u32) -> Option<std::ops::Range<usize>> {
        let bytes = frame_bytes(width, height)?;
        let slot_bytes = self.map.len().checked_sub(HEADER)? / 2;
        if slot > 1 || bytes > slot_bytes {
            return None;
        }
        let start = HEADER + slot as usize * slot_bytes;
        Some(start..start + bytes)
    }

    /// Whether the file can hold a `width x height` frame in `slot`.
    pub fn holds(&self, slot: u8, width: u32, height: u32) -> bool {
        self.span(slot, width, height).is_some()
    }

    /// Copy frame `seq` (a `width x height` frame in `slot`) into `out`.
    /// Returns false, leaving `out` unspecified, if the slot no longer (or not
    /// yet) holds that whole frame.
    pub fn read(&self, slot: u8, seq: u64, width: u32, height: u32, out: &mut Vec<u8>) -> bool {
        let Some(span) = self.span(slot, width, height) else {
            return false;
        };
        // SAFETY: the mapping is page-aligned and longer than the header.
        let word = unsafe { seq_word(self.map.as_ptr(), slot) };
        if seq == 0 || word.load(Ordering::Acquire) != seq {
            return false;
        }
        out.clear();
        out.reserve(span.len());
        // SAFETY: `span` is within the mapping. The host may be writing these
        // bytes concurrently; the sequence check below discards such a copy,
        // and plain bytes have no invalid values.
        unsafe {
            std::ptr::copy_nonoverlapping(self.map.as_ptr().add(span.start), out.as_mut_ptr(), span.len());
            out.set_len(span.len());
        }
        fence(Ordering::Acquire);
        word.load(Ordering::Relaxed) == seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-test temp directory, removed when dropped (also when the test
    /// fails).
    struct TmpDir(PathBuf);

    impl std::ops::Deref for TmpDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for TmpDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<std::ffi::OsStr> for TmpDir {
        fn as_ref(&self) -> &std::ffi::OsStr {
            self.0.as_os_str()
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmpdir(tag: &str) -> TmpDir {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "ghostrealm-frame-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_frame_dir_falls_back_to_a_fresh_private_one() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = tmpdir("fallback");
        // Open to others: refused.
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let got = frame_dir_in(&open, &base).unwrap();
        assert_ne!(got, open);
        assert!(got.starts_with(&base));
        let meta = std::fs::metadata(&got).unwrap();
        assert_eq!(meta.mode() & 0o077, 0, "private");
        assert_eq!(meta.uid(), user_id());
        // A missing parent (a stale XDG_RUNTIME_DIR) falls back too.
        let stale = base.join("gone/ghostrealm");
        assert!(frame_dir_in(&stale, &base).unwrap().starts_with(&base));
        // Each fallback is a new directory.
        assert_ne!(frame_dir_in(&open, &base).unwrap(), got);
        // A usable preferred directory is kept.
        let good = base.join("good");
        assert_eq!(frame_dir_in(&good, &base).unwrap(), good);
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[test]
    fn the_frame_file_is_allocated_not_sparse() {
        use std::os::unix::fs::MetadataExt;
        let dir = tmpdir("alloc");
        let mut w = FrameWriter::new(&dir, "t");
        let f = w.write(64, 64, &vec![1; 64 * 64 * 4]).unwrap();
        let meta = std::fs::metadata(&f.path).unwrap();
        assert!(meta.blocks() * 512 >= meta.len(), "blocks are reserved up front");
    }

    #[test]
    fn writes_alternate_slots_readable_by_a_reader() {
        let dir = tmpdir("alt");
        let mut w = FrameWriter::new(&dir, "t");
        let a = w.write(2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let b = w.write(2, 1, &[9; 8]).unwrap();
        assert_ne!(a.slot, b.slot);
        assert_eq!(a.path, b.path, "same-size frames reuse the file");
        let r = FrameReader::open(&b.path).unwrap();
        let mut px = Vec::new();
        assert!(r.read(a.slot, a.seq, 2, 1, &mut px));
        assert_eq!(px, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(r.read(b.slot, b.seq, 2, 1, &mut px));
        assert_eq!(px, [9; 8]);
        assert!(!r.holds(0, 100, 100), "out-of-range frame is rejected");
        assert!(!r.read(0, 1, 100, 100, &mut px));
        // Sizes whose byte count overflows (2^31 x 2^31 x 4 wraps to 0).
        assert!(!r.holds(0, 1 << 31, 1 << 31));
        assert!(!r.read(a.slot, a.seq, 1 << 31, 1 << 31, &mut px));
        assert!(w.write(1 << 31, 1 << 31, &[0; 8]).is_err());

        // Two frames later the first slot holds a newer frame: the old one
        // is gone rather than read torn or replaced.
        let c = w.write(2, 1, &[5; 8]).unwrap();
        assert_eq!(c.slot, a.slot);
        assert!(!r.read(a.slot, a.seq, 2, 1, &mut px), "overwritten frame");
        assert!(r.read(c.slot, c.seq, 2, 1, &mut px));
        assert_eq!(px, [5; 8]);
        drop(w);
        assert!(
            !Path::new(&b.path).exists(),
            "writer removes its file on drop"
        );
    }

    /// Readers hold on to a frame while the writer keeps overwriting both
    /// slots, so the writer rewrites that frame's slot under reads in progress
    /// (more readers than cores get preempted mid-copy): those reads fail rather
    /// than return a torn or newer frame.
    #[test]
    fn a_read_never_returns_a_torn_frame() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use std::sync::Mutex;
        let dir = tmpdir("torn");
        let (w, h) = (256u32, 256u32);
        let len = (w * h * 4) as usize;
        // Frame `seq` is filled with `seq % 4`, which tells it from the frames
        // written to its slot after it.
        let frames: Vec<Vec<u8>> = (0..4).map(|n| vec![n; len]).collect();
        let mut writer = FrameWriter::new(&dir, "t");
        let first = writer.write(w, h, &frames[1]).unwrap();
        let reader = FrameReader::open(&first.path).unwrap();
        let (stop, latest) = (AtomicBool::new(false), Mutex::new(first));
        let (ok, gone, torn) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
        let readers = std::thread::available_parallelism().map_or(4, |n| n.get()) * 2;
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut seq = 2;
                while !stop.load(Ordering::Relaxed) {
                    let f = writer.write(w, h, &frames[seq % 4]).unwrap();
                    *latest.lock().unwrap() = f;
                    seq += 1;
                }
            });
            let readers: Vec<_> = (0..readers)
                .map(|_| {
                    s.spawn(|| {
                        let until = std::time::Instant::now() + std::time::Duration::from_millis(500);
                        let mut px = Vec::new();
                        while std::time::Instant::now() < until {
                            // Re-read one frame until its slot is rewritten.
                            let f = latest.lock().unwrap().clone();
                            while reader.read(f.slot, f.seq, w, h, &mut px) {
                                if px != frames[f.seq as usize % 4] {
                                    torn.fetch_add(1, Ordering::Relaxed);
                                }
                                ok.fetch_add(1, Ordering::Relaxed);
                            }
                            gone.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            for r in readers {
                r.join().unwrap();
            }
            stop.store(true, Ordering::Relaxed);
        });
        let (ok, gone, torn) = (ok.into_inner(), gone.into_inner(), torn.into_inner());
        assert_eq!(torn, 0, "wrong frames returned ({ok} right, {gone} overwritten)");
        assert!(ok > 0, "some reads succeed");
        assert!(gone > 0, "some reads raced the writer");
    }

    #[cfg(unix)]
    #[test]
    fn frame_files_are_private_and_never_reused_through_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("private");
        let target = dir.join("victim");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("p-1.bgra")).unwrap();
        let mut w = FrameWriter::new(&dir, "p");
        let f = w.write(1, 1, &[1; 4]).unwrap();
        let mode = std::fs::metadata(&f.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep", "the symlink's target is untouched");
        assert!(!std::fs::symlink_metadata(&f.path).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn the_frame_dir_must_be_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmpdir("dir");
        let fresh = base.join("fresh");
        let _ = std::fs::remove_dir(&fresh);
        private_dir(&fresh).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "created private");
        private_dir(&fresh).expect("an existing private dir is reused");

        let open = base.join("open");
        std::fs::create_dir_all(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&open).is_err(), "others can read it");

        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        assert!(private_dir(&link).is_err(), "a symlink, even to a private dir");

        // Another user's directory: root owns `/`.
        assert!(private_dir(Path::new("/")).is_err());
        assert!(frame_dir().unwrap().is_dir());
    }

    /// Given a temp dir other than the environment's;
    /// then the frame dir is made from it (or `XDG_RUNTIME_DIR`), not from
    /// the environment's, which the host changes for Chromium.
    #[test]
    fn the_frame_dir_comes_from_the_temp_dir_given() {
        let base = tmpdir("given");
        let dir = frame_dir_from(&base).unwrap();
        let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute());
        match run {
            Some(run) if dir.starts_with(&run) => assert_eq!(dir, run.join("ghostrealm")),
            _ => assert!(dir.starts_with(&*base), "{} is not under {}", dir.display(), base.display()),
        }
        assert!(dir.is_dir());
    }

    #[test]
    fn stale_frame_files_of_dead_hosts_are_removed() {
        let dir = tmpdir("stale");
        for name in [
            "ghostrealm-web-100-1-1.bgra",
            "ghostrealm-web-200-3-2.bgra",
            "ghostrealm-web-x-1-1.bgra",
            "other.bgra",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        remove_stale(&dir, |pid| pid == 200);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["ghostrealm-web-200-3-2.bgra", "ghostrealm-web-x-1-1.bgra", "other.bgra"]);
    }

    #[test]
    fn a_frame_file_that_cannot_be_sized_is_not_left_behind() {
        let dir = tmpdir("leak");
        let mut w = FrameWriter::new(&dir, "l");
        // Beyond any address space: sizing or mapping fails.
        assert!(w.grow(1 << 46).is_err());
        assert!(w.grow(1 << 46).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn fallback_dirs_of_dead_hosts_are_cleared() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmpdir("sweep");
        let uid = user_id();
        let make = |name: String, mode: u32, files: &[&str]| {
            let d = base.join(name);
            std::fs::create_dir(&d).unwrap();
            for f in files {
                std::fs::write(d.join(f), b"").unwrap();
            }
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
            d
        };
        let dead = make(format!("ghostrealm-{uid}-100-aa"), 0o700, &["ghostrealm-web-100-1-1.bgra"]);
        let live = make(format!("ghostrealm-{uid}-200-bb"), 0o700, &["ghostrealm-web-200-1-1.bgra"]);
        let live_empty = make(format!("ghostrealm-{uid}-200-cc"), 0o700, &[]);
        let foreign_file = make(format!("ghostrealm-{uid}-100-dd"), 0o700, &["notes.txt"]);
        let open = make(format!("ghostrealm-{uid}-100-ee"), 0o755, &[]);
        let other_user = make(format!("ghostrealm-{}-100-ff", uid + 1), 0o700, &[]);
        let preferred = make(format!("ghostrealm-{uid}"), 0o700, &[]);
        remove_stale_dirs(&base, |pid| pid == 200);
        assert!(!dead.exists(), "dead host's dir and frames are removed");
        assert!(live.join("ghostrealm-web-200-1-1.bgra").exists());
        assert!(live_empty.exists());
        assert!(foreign_file.join("notes.txt").exists(), "unknown files keep the dir");
        for kept in [&open, &other_user, &preferred] {
            assert!(kept.exists(), "{} is not ours to remove", kept.display());
        }
        // A fallback dir is named after its host.
        let mine = random_private_dir(&base).unwrap();
        remove_stale_dirs(&base, |pid| pid == std::process::id());
        assert!(mine.exists());
    }

    #[test]
    fn growing_replaces_the_file_without_breaking_an_old_mapping() {
        let dir = tmpdir("grow");
        let mut w = FrameWriter::new(&dir, "g");
        let small = w.write(1, 1, &[7; 4]).unwrap();
        let old = FrameReader::open(&small.path).unwrap();
        let big = w.write(4, 4, &[3; 64]).unwrap();
        assert_ne!(small.path, big.path);
        assert!(!Path::new(&small.path).exists(), "old file is unlinked");
        let mut px = Vec::new();
        assert!(old.read(small.slot, small.seq, 1, 1, &mut px));
        assert_eq!(px, [7; 4], "old mapping still readable");
        let new = FrameReader::open(&big.path).unwrap();
        assert!(new.read(big.slot, big.seq, 4, 4, &mut px));
        assert_eq!(px, [3; 64]);
    }
}
