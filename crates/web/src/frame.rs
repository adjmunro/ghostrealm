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
        let bytes = width as usize * height as usize * 4;
        if pixels.len() < bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "short frame buffer",
            ));
        }
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
        file.set_len((HEADER + bytes * 2) as u64)?;
        // SAFETY: the file is private to this writer (fresh, unique name) and is
        // never truncated while mapped.
        let map = unsafe { MmapMut::map_mut(&file)? };
        if let Some((old, _)) = self.current.replace((path, map)) {
            let _ = std::fs::remove_file(old);
        }
        Ok(())
    }
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
        let bytes = width as usize * height as usize * 4;
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

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ghostrealm-frame-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
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
