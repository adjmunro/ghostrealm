//! [`AudioBackend`](super::AudioBackend) on `rodio` (cpal output, symphonia
//! decoders: flac, mp3, mp4/aac, vorbis, wav), as the manifest's features select.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use super::{AudioBackend, AudioDevice, Track};

/// Plays through `rodio`.
pub struct Rodio;

impl AudioBackend for Rodio {
    fn open_device(&self) -> Result<Box<dyn AudioDevice>, String> {
        let mut d = DeviceSinkBuilder::open_default_sink().map_err(|e| format!("no audio output: {e}"))?;
        d.log_on_drop(false);
        Ok(Box::new(RodioDevice(d)))
    }

    fn duration(&self, path: &Path) -> Result<Option<Duration>, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let source = Decoder::try_from(file).map_err(|e| format!("can't decode: {e}"))?;
        Ok(source.total_duration())
    }
}

struct RodioDevice(MixerDeviceSink);

impl AudioDevice for RodioDevice {
    fn play(&self, path: &Path) -> Result<(Box<dyn Track>, Option<Duration>), String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let source = Decoder::try_from(file).map_err(|e| format!("can't decode: {e}"))?;
        let total = source.total_duration();
        let player = Player::connect_new(self.0.mixer());
        player.append(source);
        Ok((Box::new(RodioTrack(player)), total))
    }
}

struct RodioTrack(Player);

impl Track for RodioTrack {
    fn pause(&self) {
        self.0.pause();
    }

    fn resume(&self) {
        self.0.play();
    }

    fn is_paused(&self) -> bool {
        self.0.is_paused()
    }

    fn is_finished(&self) -> bool {
        self.0.empty()
    }

    fn position(&self) -> Duration {
        self.0.get_pos()
    }

    fn seek(&self, pos: Duration) {
        let _ = self.0.try_seek(pos);
    }
}
