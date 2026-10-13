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
        // SAFETY: the mapping is page-aligned and longer than the header.
        let word = unsafe { seq_word(map.as_ptr(), slot) };
        word.store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        map[start..start + bytes].copy_from_slice(&pixels[..bytes]);
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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
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

    /// While the writer keeps overwriting both slots, every read that succeeds
    /// is one whole frame.
    #[test]
    fn a_read_never_returns_a_torn_frame() {
        let dir = tmpdir("torn");
        let (w, h) = (256u32, 256u32);
        let mut writer = FrameWriter::new(&dir, "t");
        let first = writer.write(w, h, &vec![0; (w * h * 4) as usize]).unwrap();
        let reader = FrameReader::open(&first.path).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let latest = std::sync::Arc::new(std::sync::Mutex::new(first.clone()));
        let t = {
            let (stop, latest) = (stop.clone(), latest.clone());
            std::thread::spawn(move || {
                let mut n = 1u8;
                while !stop.load(Ordering::Relaxed) {
                    let f = writer.write(w, h, &vec![n; (w * h * 4) as usize]).unwrap();
                    *latest.lock().unwrap() = f;
                    n = n.wrapping_add(1);
                }
            })
        };
        let (mut ok, mut gone) = (0, 0);
        let mut px = Vec::new();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < until {
            let f = latest.lock().unwrap().clone();
            if reader.read(f.slot, f.seq, w, h, &mut px) {
                assert!(px.iter().all(|b| *b == px[0]), "torn frame {}", f.seq);
                ok += 1;
            } else {
                gone += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(ok > 0, "some reads succeed ({gone} overwritten)");
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
