//! Audio playback. A thread of its own owns the output device
//! (opening it can take tens of ms, and the device handle isn't `Send` on every
//! platform); the view sends it commands and reads back a [`Status`]. Dropping
//! the [`AudioPlayer`] ends the thread, which stops playback.
//!
//! The device is open only while it is in use: an open output keeps the system
//! from idle-sleeping, so it closes once playback has been paused or finished for
//! [`IDLE_CLOSE`], and reopens on Play or Resume.
//!
//! The player drives an [`AudioBackend`] (device, decoder and mixer), which is
//! the seam to the playback crate: [`Rodio`] is the real one, and nothing else
//! names `rodio`. Closing the device is dropping the [`AudioDevice`].

mod rodio_backend;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use rodio_backend::Rodio;

/// Opens audio output and reads track metadata.
pub trait AudioBackend: Send + 'static {
    /// Open the default output device. Errors read as "no audio output: ...".
    fn open_device(&self) -> Result<Box<dyn AudioDevice>, String>;
    /// The length of the track at `path`, if its format records one.
    fn duration(&self, path: &Path) -> Result<Option<Duration>, String>;
}

/// An open output device. Dropping it closes the device and stops its tracks.
pub trait AudioDevice {
    /// Start the track at `path` playing from the start. Returns it and its
    /// length when the format records it.
    fn play(&self, path: &Path) -> Result<(Box<dyn Track>, Option<Duration>), String>;
}

/// A track on a device.
pub trait Track {
    fn pause(&self);
    fn resume(&self);
    fn is_paused(&self) -> bool;
    /// Played to the end (or never had anything to play).
    fn is_finished(&self) -> bool;
    fn position(&self) -> Duration;
    /// Jump to `pos`. A format that can't seek ignores it.
    fn seek(&self, pos: Duration);
}

/// How often the thread refreshes [`Status`] while a track plays.
const STATUS_EVERY: Duration = Duration::from_millis(50);
/// How long the device stays open with nothing playing.
const IDLE_CLOSE: Duration = Duration::from_secs(10);

/// What the player is doing, as of its last refresh.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// The loaded track.
    pub path: Option<PathBuf>,
    pub pos: Duration,
    pub total: Option<Duration>,
    /// The track hasn't finished: it can pause and resume.
    pub loaded: bool,
    pub playing: bool,
    /// The device or the file couldn't be opened.
    pub error: Option<String>,
    /// The output device is open.
    pub device_open: bool,
}

enum Cmd {
    /// Load a track (replacing the current one) and play it.
    Play(PathBuf),
    Pause,
    Resume,
    Seek(Duration),
}

pub struct AudioPlayer {
    tx: Sender<Cmd>,
    status: Arc<Mutex<Status>>,
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self::with_backend(Rodio, IDLE_CLOSE)
    }

    /// A player that plays through `backend`, closing the device after `idle`
    /// with nothing playing.
    pub fn with_backend(backend: impl AudioBackend, idle: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let shared = status.clone();
        std::thread::Builder::new()
            .name("preview-audio".into())
            .spawn(move || run(rx, shared, idle, &backend))
            .expect("spawn preview audio thread");
        AudioPlayer { tx, status }
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Play `path` from the start.
    pub fn play(&self, path: &Path) {
        if let Ok(mut s) = self.status.lock() {
            // Reflect the request at once, so the next paint shows it.
            *s = Status {
                path: Some(path.to_path_buf()),
                loaded: true,
                playing: true,
                ..Status::default()
            };
        }
        let _ = self.tx.send(Cmd::Play(path.to_path_buf()));
    }

    pub fn pause(&self) {
        if let Ok(mut s) = self.status.lock() {
            s.playing = false;
        }
        let _ = self.tx.send(Cmd::Pause);
    }

    pub fn resume(&self) {
        if let Ok(mut s) = self.status.lock() {
            s.playing = true;
        }
        let _ = self.tx.send(Cmd::Resume);
    }

    pub fn seek(&self, pos: Duration) {
        if let Ok(mut s) = self.status.lock() {
            s.pos = pos;
        }
        let _ = self.tx.send(Cmd::Seek(pos));
    }
}

impl Default for AudioPlayer {
    fn default() -> Self {
        Self::new()
    }
}

fn run(rx: Receiver<Cmd>, status: Arc<Mutex<Status>>, idle_close: Duration, backend: &dyn AudioBackend) {
    let mut device: Option<Box<dyn AudioDevice>> = None;
    let mut player: Option<Box<dyn Track>> = None;
    // A paused track whose device was closed: its path and position.
    let mut parked: Option<(PathBuf, Duration)> = None;
    // The loaded track's path.
    let mut current: Option<PathBuf> = None;
    let mut idle_since: Option<Instant> = None;
    loop {
        let playing = player.as_ref().is_some_and(|p| !p.is_paused() && !p.is_finished());
        if playing {
            idle_since = None;
        } else if device.is_some() {
            idle_since.get_or_insert_with(Instant::now);
        }
        // Playing: refresh the position too. Idle with the device open: wake to
        // close it. Otherwise block for the next command.
        let wait = if playing {
            Some(STATUS_EVERY)
        } else {
            idle_since.map(|t| idle_close.saturating_sub(t.elapsed()))
        };
        let cmd = match wait {
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
        };
        let mut error = None;
        match cmd {
            Some(Cmd::Play(path)) => {
                player = None;
                parked = None;
                current = None;
                match start(backend, &mut device, &path) {
                    Ok((p, total)) => {
                        player = Some(p);
                        current = Some(path);
                        if let Ok(mut s) = status.lock() {
                            s.total = total;
                        }
                    }
                    Err(e) => error = Some(e),
                }
            }
            Some(Cmd::Pause) => {
                if let Some(p) = &player {
                    p.pause();
                }
            }
            Some(Cmd::Resume) => match (&player, parked.take()) {
                (Some(p), _) => p.resume(),
                (None, Some((path, pos))) => match start(backend, &mut device, &path) {
                    Ok((p, _)) => {
                        p.seek(pos);
                        player = Some(p);
                        current = Some(path);
                    }
                    Err(e) => error = Some(e),
                },
                (None, None) => {}
            },
            Some(Cmd::Seek(pos)) => {
                if let Some(p) = &player {
                    p.seek(pos);
                }
                if let Some((_, at)) = &mut parked {
                    *at = pos;
                }
            }
            None => {}
        }
        if idle_since.is_some_and(|t| t.elapsed() >= idle_close)
            && !player.as_ref().is_some_and(|p| !p.is_paused() && !p.is_finished())
        {
            // Idle long enough: close the device, keeping a paused track's place.
            if let (Some(p), Some(path)) = (&player, current.take()) {
                if !p.is_finished() {
                    parked = Some((path, p.position()));
                }
            }
            player = None;
            device = None;
            idle_since = None;
        }
        if let Ok(mut s) = status.lock() {
            if let Some(p) = &player {
                s.pos = p.position();
                s.loaded = !p.is_finished();
                s.playing = s.loaded && !p.is_paused();
            } else if let Some((_, pos)) = &parked {
                s.pos = *pos;
                s.loaded = true;
                s.playing = false;
            } else {
                s.loaded = false;
                s.playing = false;
            }
            s.device_open = device.is_some();
            if error.is_some() {
                s.error = error;
            }
        }
    }
}

/// Open the device if needed and start `path` playing. Returns the track and
/// its length when the format records it.
fn start(
    backend: &dyn AudioBackend,
    device: &mut Option<Box<dyn AudioDevice>>,
    path: &Path,
) -> Result<(Box<dyn Track>, Option<Duration>), String> {
    if device.is_none() {
        *device = Some(backend.open_device()?);
    }
    device.as_ref().ok_or("no audio output")?.play(path)
}

/// The length of the track at `path`, if its format records one.
pub fn duration(path: &Path) -> Result<Option<Duration>, String> {
    Rodio.duration(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::tmpdir;
    use crate::media::testing::wav;

    /// Needs an audio output device; plays silence. Run with `--ignored
    /// --test-threads 1` (two players opening the device at once can stall).
    #[test]
    #[ignore]
    fn plays_pauses_and_finishes_through_the_default_output() {
        let d = tmpdir("audio");
        let path = d.join("silence.wav");
        std::fs::write(&path, wav(8000, 8000)).unwrap();
        let player = AudioPlayer::new();
        player.play(&path);
        // A cold first open of the device can take a few hundred ms.
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while player.status().pos.is_zero() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        let st = player.status();
        assert_eq!(st.error, None);
        assert!(st.playing && st.loaded, "{st:?}");
        assert!(st.pos > Duration::ZERO && st.total == Some(Duration::from_secs(1)), "{st:?}");
        player.pause();
        std::thread::sleep(Duration::from_millis(100));
        let paused = player.status();
        assert!(!paused.playing && paused.loaded, "{paused:?}");
        player.resume();
        std::thread::sleep(Duration::from_millis(1200));
        let done = player.status();
        assert!(!done.playing && !done.loaded, "finished: {done:?}");
    }

    fn wait_for(player: &AudioPlayer, what: &str, ok: impl Fn(&Status) -> bool) -> Status {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let st = player.status();
            if ok(&st) {
                return st;
            }
            assert!(std::time::Instant::now() < until, "{what}: {st:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Needs an audio output device; plays silence. Run with `--ignored
    /// --test-threads 1` (two players opening the device at once can stall).
    #[test]
    #[ignore]
    fn the_device_closes_when_idle_and_a_paused_track_resumes_in_place() {
        let d = tmpdir("audio-idle");
        let short = d.join("short.wav");
        let long = d.join("long.wav");
        std::fs::write(&short, wav(8000, 2000)).unwrap();
        std::fs::write(&long, wav(8000, 8000 * 5)).unwrap();
        let player = AudioPlayer::with_backend(Rodio, Duration::from_millis(300));

        player.play(&short);
        wait_for(&player, "plays", |s| s.device_open);
        wait_for(&player, "closes after finishing", |s| !s.device_open && !s.loaded);

        player.play(&long);
        let st = wait_for(&player, "plays", |s| s.device_open && s.pos > Duration::from_millis(300));
        player.pause();
        let parked = wait_for(&player, "closes while paused", |s| !s.device_open);
        assert!(parked.loaded && !parked.playing && parked.pos >= st.pos, "{parked:?}");
        player.resume();
        let back = wait_for(&player, "reopens on resume", |s| s.device_open && s.playing);
        assert!(back.pos >= parked.pos, "resumes in place: {back:?} after {parked:?}");
    }

    /// What a [`Fake`] backend was asked to do, and the state of its tracks.
    #[derive(Default)]
    struct Log {
        /// Devices opened so far, and how many are open now.
        opened: usize,
        open: usize,
        /// Every track started, in order: its path and its state.
        tracks: Vec<(PathBuf, Arc<Mutex<FakeTrackState>>)>,
        /// Opening the device fails.
        no_output: bool,
    }

    /// A fake track's state: set by the test, read by the player.
    #[derive(Default)]
    struct FakeTrackState {
        paused: bool,
        finished: bool,
        pos: Duration,
        seeks: Vec<Duration>,
        dropped: bool,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Log>>);

    struct FakeDevice(Arc<Mutex<Log>>);
    struct FakeTrack(Arc<Mutex<FakeTrackState>>);

    impl AudioBackend for Fake {
        fn open_device(&self) -> Result<Box<dyn AudioDevice>, String> {
            let mut log = self.0.lock().unwrap();
            if log.no_output {
                return Err("no audio output: fake".into());
            }
            log.opened += 1;
            log.open += 1;
            Ok(Box::new(FakeDevice(self.0.clone())))
        }

        fn duration(&self, _: &Path) -> Result<Option<Duration>, String> {
            Ok(Some(Duration::from_secs(60)))
        }
    }

    impl Drop for FakeDevice {
        fn drop(&mut self) {
            self.0.lock().unwrap().open -= 1;
        }
    }

    impl AudioDevice for FakeDevice {
        fn play(&self, path: &Path) -> Result<(Box<dyn Track>, Option<Duration>), String> {
            let state = Arc::new(Mutex::new(FakeTrackState::default()));
            self.0.lock().unwrap().tracks.push((path.to_path_buf(), state.clone()));
            Ok((Box::new(FakeTrack(state)), Some(Duration::from_secs(60))))
        }
    }

    impl Drop for FakeTrack {
        fn drop(&mut self) {
            self.0.lock().unwrap().dropped = true;
        }
    }

    impl Track for FakeTrack {
        fn pause(&self) {
            self.0.lock().unwrap().paused = true;
        }
        fn resume(&self) {
            self.0.lock().unwrap().paused = false;
        }
        fn is_paused(&self) -> bool {
            self.0.lock().unwrap().paused
        }
        fn is_finished(&self) -> bool {
            self.0.lock().unwrap().finished
        }
        fn position(&self) -> Duration {
            self.0.lock().unwrap().pos
        }
        fn seek(&self, pos: Duration) {
            let mut t = self.0.lock().unwrap();
            t.seeks.push(pos);
            t.pos = pos;
        }
    }

    const IDLE: Duration = Duration::from_millis(120);

    fn fake_player() -> (AudioPlayer, Fake) {
        let fake = Fake::default();
        (AudioPlayer::with_backend(fake.clone(), IDLE), fake)
    }

    /// Wait for the player's thread to bring track 0 to a state.
    fn wait_for_track(fake: &Fake, what: &str, ok: impl Fn(&FakeTrackState) -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !ok(&track(fake, 0).lock().unwrap()) {
            assert!(Instant::now() < until, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn track(fake: &Fake, n: usize) -> Arc<Mutex<FakeTrackState>> {
        fake.0.lock().unwrap().tracks[n].1.clone()
    }

    #[test]
    fn play_opens_the_device_and_reports_the_track() {
        let (player, fake) = fake_player();
        player.play(Path::new("a.mp3"));
        let st = wait_for(&player, "plays", |s| s.device_open && s.total.is_some());
        assert_eq!(st.path, Some(PathBuf::from("a.mp3")));
        assert!(st.playing && st.loaded && st.error.is_none(), "{st:?}");
        assert_eq!(st.total, Some(Duration::from_secs(60)));
        track(&fake, 0).lock().unwrap().pos = Duration::from_secs(5);
        wait_for(&player, "position follows the track", |s| s.pos == Duration::from_secs(5));
    }

    #[test]
    fn pause_and_resume_drive_the_track() {
        let (player, fake) = fake_player();
        player.play(Path::new("a.mp3"));
        wait_for(&player, "plays", |s| s.device_open && s.playing);
        player.pause();
        wait_for_track(&fake, "pauses", |t| t.paused);
        assert!(!player.status().playing);
        player.resume();
        wait_for_track(&fake, "resumes", |t| !t.paused);
        wait_for(&player, "resumes", |s| s.playing);
        assert_eq!(fake.0.lock().unwrap().opened, 1, "no reopen while the device is in use");
    }

    #[test]
    fn a_paused_track_closes_the_device_and_resumes_in_place() {
        let (player, fake) = fake_player();
        player.play(Path::new("a.mp3"));
        wait_for(&player, "plays", |s| s.device_open && s.playing);
        track(&fake, 0).lock().unwrap().pos = Duration::from_secs(7);
        wait_for(&player, "position seen", |s| s.pos == Duration::from_secs(7));
        player.pause();
        let parked = wait_for(&player, "closes while paused", |s| !s.device_open);
        assert!(parked.loaded && !parked.playing, "{parked:?}");
        assert_eq!(parked.pos, Duration::from_secs(7), "the place is kept");
        assert_eq!(fake.0.lock().unwrap().open, 0, "the device is closed");
        assert!(track(&fake, 0).lock().unwrap().dropped, "and its track");

        player.resume();
        let back = wait_for(&player, "reopens on resume", |s| s.device_open && s.playing);
        assert_eq!(back.pos, Duration::from_secs(7));
        let log = fake.0.lock().unwrap();
        assert_eq!(log.opened, 2);
        assert_eq!(log.tracks.len(), 2);
        assert_eq!(log.tracks[1].0, PathBuf::from("a.mp3"));
        assert_eq!(log.tracks[1].1.lock().unwrap().seeks, vec![Duration::from_secs(7)]);
    }

    #[test]
    fn seeking_a_parked_track_moves_where_it_resumes() {
        let (player, fake) = fake_player();
        player.play(Path::new("a.mp3"));
        wait_for(&player, "plays", |s| s.device_open && s.playing);
        player.pause();
        wait_for(&player, "parked", |s| !s.device_open);
        player.seek(Duration::from_secs(30));
        wait_for(&player, "seek shown", |s| s.pos == Duration::from_secs(30));
        player.resume();
        wait_for(&player, "reopens", |s| s.device_open && s.playing);
        assert_eq!(track(&fake, 1).lock().unwrap().seeks, vec![Duration::from_secs(30)]);
    }

    #[test]
    fn a_finished_track_is_not_loaded_and_closes_the_device() {
        let (player, fake) = fake_player();
        player.play(Path::new("a.mp3"));
        wait_for(&player, "plays", |s| s.device_open && s.playing);
        track(&fake, 0).lock().unwrap().finished = true;
        let st = wait_for(&player, "closes after finishing", |s| !s.device_open);
        assert!(!st.loaded && !st.playing, "{st:?}");
        player.resume();
        std::thread::sleep(Duration::from_millis(60));
        let st = player.status();
        assert_eq!(fake.0.lock().unwrap().opened, 1, "nothing to resume: no reopen");
        assert!(!st.device_open, "{st:?}");
    }

    #[test]
    fn playing_another_track_replaces_the_first() {
        let (player, fake) = fake_player();
        player.play(Path::new("a.mp3"));
        wait_for(&player, "plays a", |s| s.device_open && s.playing);
        player.play(Path::new("b.mp3"));
        wait_for(&player, "plays b", |s| s.path == Some(PathBuf::from("b.mp3")) && s.total.is_some());
        let log = fake.0.lock().unwrap();
        assert!(log.tracks[0].1.lock().unwrap().dropped, "the first stopped");
        assert_eq!((log.tracks.len(), log.opened), (2, 1), "on the same device");
    }

    #[test]
    fn a_device_that_cannot_open_is_an_error() {
        let (player, fake) = fake_player();
        fake.0.lock().unwrap().no_output = true;
        player.play(Path::new("a.mp3"));
        let st = wait_for(&player, "error shown", |s| s.error.is_some());
        assert_eq!(st.error.as_deref(), Some("no audio output: fake"));
        assert!(!st.playing && !st.loaded && !st.device_open, "{st:?}");
    }

    #[test]
    fn dropping_the_player_closes_the_device() {
        let (player, fake) = fake_player();
        player.play(Path::new("a.mp3"));
        wait_for(&player, "plays", |s| s.device_open && s.playing);
        drop(player);
        let until = Instant::now() + Duration::from_secs(2);
        while fake.0.lock().unwrap().open != 0 {
            assert!(Instant::now() < until, "the device stayed open");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_rodio_backend_reports_a_wav_length_without_a_device() {
        let d = tmpdir("audio-duration");
        std::fs::write(d.join("a.wav"), wav(8000, 8000 * 2)).unwrap();
        assert_eq!(duration(&d.join("a.wav")), Ok(Some(Duration::from_secs(2))));
        std::fs::write(d.join("bad.wav"), b"nope").unwrap();
        assert!(duration(&d.join("bad.wav")).is_err());
        assert!(duration(&d.join("missing.wav")).is_err());
    }
}
