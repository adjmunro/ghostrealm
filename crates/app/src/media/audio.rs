//! Audio playback. A thread of its own owns the output device
//! (opening it can take tens of ms, and the device handle isn't `Send` on every
//! platform); the view sends it commands and reads back a [`Status`]. Dropping
//! the [`AudioPlayer`] ends the thread, which stops playback.
//!
//! The device is open only while it is in use: an open output keeps the system
//! from idle-sleeping, so it closes once playback has been paused or finished for
//! [`IDLE_CLOSE`], and reopens on Play or Resume.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

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
        Self::with_idle_close(IDLE_CLOSE)
    }

    /// A player that closes the device after `idle` with nothing playing.
    fn with_idle_close(idle: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let shared = status.clone();
        std::thread::Builder::new()
            .name("preview-audio".into())
            .spawn(move || run(rx, shared, idle))
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

fn run(rx: Receiver<Cmd>, status: Arc<Mutex<Status>>, idle_close: Duration) {
    let mut device: Option<MixerDeviceSink> = None;
    let mut player: Option<Player> = None;
    // A paused track whose device was closed: its path and position.
    let mut parked: Option<(PathBuf, Duration)> = None;
    // The loaded track's path.
    let mut current: Option<PathBuf> = None;
    let mut idle_since: Option<Instant> = None;
    loop {
        let playing = player.as_ref().is_some_and(|p| !p.is_paused() && !p.empty());
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
                match start(&mut device, &path) {
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
                (Some(p), _) => p.play(),
                (None, Some((path, pos))) => match start(&mut device, &path) {
                    Ok((p, _)) => {
                        let _ = p.try_seek(pos);
                        player = Some(p);
                        current = Some(path);
                    }
                    Err(e) => error = Some(e),
                },
                (None, None) => {}
            },
            Some(Cmd::Seek(pos)) => {
                if let Some(p) = &player {
                    let _ = p.try_seek(pos);
                }
                if let Some((_, at)) = &mut parked {
                    *at = pos;
                }
            }
            None => {}
        }
        if idle_since.is_some_and(|t| t.elapsed() >= idle_close)
            && !player.as_ref().is_some_and(|p| !p.is_paused() && !p.empty())
        {
            // Idle long enough: close the device, keeping a paused track's place.
            if let (Some(p), Some(path)) = (&player, current.take()) {
                if !p.empty() {
                    parked = Some((path, p.get_pos()));
                }
            }
            player = None;
            device = None;
            idle_since = None;
        }
        if let Ok(mut s) = status.lock() {
            if let Some(p) = &player {
                s.pos = p.get_pos();
                s.loaded = !p.empty();
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

/// Open the device if needed and start `path` playing. Returns the player and
/// the track's length when the format records it.
fn start(device: &mut Option<MixerDeviceSink>, path: &Path) -> Result<(Player, Option<Duration>), String> {
    if device.is_none() {
        let mut d = DeviceSinkBuilder::open_default_sink().map_err(|e| format!("no audio output: {e}"))?;
        d.log_on_drop(false);
        *device = Some(d);
    }
    let mixer = device.as_ref().map(|d| d.mixer()).ok_or("no audio output")?;
    let file = File::open(path).map_err(|e| e.to_string())?;
    let source = Decoder::try_from(file).map_err(|e| format!("can't decode: {e}"))?;
    let total = source.total_duration();
    let player = Player::connect_new(mixer);
    player.append(source);
    Ok((player, total))
}

/// The length of the track at `path`, if its format records one.
pub fn duration(path: &Path) -> Result<Option<Duration>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let source = Decoder::try_from(file).map_err(|e| format!("can't decode: {e}"))?;
    Ok(source.total_duration())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::tmpdir;
    use crate::plugins::file_browser::preview::tests::wav;

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
        let player = AudioPlayer::with_idle_close(Duration::from_millis(300));

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
}
