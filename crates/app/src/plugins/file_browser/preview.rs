//! The file browser's preview of the highlighted entry: its details, plus a
//! scrollable head of a text file, an image, or an audio player. Reading and
//! decoding run on a worker; the view polls for the result on a timer, keeping
//! the previous preview up until the new one is ready.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ghostrealm_core::highlight::{self, Span};
use ghostrealm_core::{Config, Rect};
use glyphon::Family;

use crate::media::audio::{self, AudioPlayer};
use crate::media;
use crate::plugin::{rect_contains, theme, EventCx, Font, Image, PaintCx};
use crate::syntax;

/// Preview button ids, after the browser header's (see `View::button`).
pub const BTN_PLAY: u32 = 10;
pub const BTN_AUTOPLAY: u32 = 11;
/// The preview's image within its view (see `PaintCx::image`).
const IMAGE_ID: u32 = 0;

/// How much of a text file to read, and how many lines (and chars per line) to
/// keep of it.
const TEXT_MAX_BYTES: u64 = 64 * 1024;
const TEXT_MAX_LINES: usize = 1000;
const TEXT_MAX_COLS: usize = 400;
/// Images larger than this on either side are downscaled when decoded.
const IMAGE_MAX_SIDE: u32 = 2048;
/// How often to check for a pending load's result.
const POLL_EVERY: Duration = Duration::from_millis(15);
/// How long audio must stay previewed before autoplay starts it (so sweeping
/// the highlight across tracks stays quiet).
const AUTOPLAY_AFTER: Duration = Duration::from_millis(400);
/// How often the playback position redraws.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

const IMAGE_EXTS: &[&str] = &["bmp", "gif", "ico", "jpeg", "jpg", "png", "tif", "tiff", "webp"];
const AUDIO_EXTS: &[&str] = &["flac", "m4a", "mp3", "oga", "ogg", "wav"];

/// What the preview shows below an entry's details.
#[derive(Clone)]
pub enum Content {
    Dir { entries: usize },
    /// `spans` is each line's syntax highlight, empty for a file with no
    /// highlighter.
    Text { lines: Vec<String>, spans: Vec<Vec<Span>>, truncated: bool },
    /// `width` x `height` is the file's size; `image` may be downscaled.
    Image { image: Arc<Image>, width: u32, height: u32 },
    Audio { duration: Option<Duration> },
    /// Binary, or a kind with no preview.
    Nothing,
    Error(String),
}

/// An entry's loaded preview.
#[derive(Clone)]
pub struct Preview {
    pub path: PathBuf,
    pub kind: String,
    /// Files only.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub content: Content,
}

/// Read and decode `path`'s preview. Runs on the loader thread.
pub fn load(path: &Path) -> Preview {
    let meta = std::fs::metadata(path);
    let modified = meta.as_ref().ok().and_then(|m| m.modified().ok());
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let upper = ext.to_uppercase();
    let preview = |kind: String, size: Option<u64>, content: Content| Preview {
        path: path.to_path_buf(),
        kind,
        size,
        modified,
        content,
    };
    let meta = match meta {
        Ok(m) => m,
        Err(e) => return preview("Missing".into(), None, Content::Error(e.to_string())),
    };
    if meta.is_dir() {
        let content = match std::fs::read_dir(path) {
            Ok(rd) => Content::Dir { entries: rd.count() },
            Err(e) => Content::Error(e.to_string()),
        };
        return preview("Folder".into(), None, content);
    }
    if !meta.is_file() {
        // Opening a pipe or device can block forever (and reading eats a
        // pipe's data): describe it without touching its content.
        return preview(special_kind(&meta).into(), None, Content::Nothing);
    }
    let size = Some(meta.len());
    if IMAGE_EXTS.contains(&ext.as_str()) {
        let content = match decode_image(path) {
            Ok(c) => c,
            Err(e) => Content::Error(e),
        };
        return preview(format!("{upper} image"), size, content);
    }
    if AUDIO_EXTS.contains(&ext.as_str()) {
        let content = match audio::duration(path) {
            Ok(duration) => Content::Audio { duration },
            Err(e) => Content::Error(e),
        };
        return preview(format!("{upper} audio"), size, content);
    }
    match read_text(path) {
        Ok(Some(content)) if ext.is_empty() => preview("Text".into(), size, content),
        Ok(Some(content)) => preview(format!("{upper} text"), size, content),
        Ok(None) if ext.is_empty() => preview("File".into(), size, Content::Nothing),
        Ok(None) => preview(format!("{upper} file"), size, Content::Nothing),
        Err(e) => preview("File".into(), size, Content::Error(e)),
    }
}

/// What a non-regular, non-directory entry is.
fn special_kind(meta: &std::fs::Metadata) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let t = meta.file_type();
        if t.is_fifo() {
            return "Pipe";
        }
        if t.is_socket() {
            return "Socket";
        }
        if t.is_char_device() || t.is_block_device() {
            return "Device";
        }
    }
    let _ = meta;
    "Special file"
}

/// Decode an image, upright per its EXIF orientation and at most
/// [`IMAGE_MAX_SIDE`] on either side.
fn decode_image(path: &Path) -> Result<Content, String> {
    let d = media::image::decode(path, IMAGE_MAX_SIDE)?;
    let image = Image::from_rgba(d.width, d.height, d.rgba);
    Ok(Content::Image { image, width: d.source_width, height: d.source_height })
}

/// The head of a text file, or `None` if it looks binary (a NUL byte, or
/// invalid UTF-8).
fn read_text(path: &Path) -> Result<Option<Content>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(TEXT_MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    let mut truncated = bytes.len() as u64 > TEXT_MAX_BYTES;
    bytes.truncate(TEXT_MAX_BYTES as usize);
    if bytes.contains(&0) {
        return Ok(None);
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(t) => t,
        // Cut mid-character at the read limit: keep what's whole.
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or_default(),
        Err(_) => return Ok(None),
    };
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if lines.len() == TEXT_MAX_LINES {
            truncated = true;
            break;
        }
        let line = line.replace('\t', "    ");
        lines.push(line.chars().take(TEXT_MAX_COLS).collect());
    }
    let spans = highlight::for_path(path).map(|h| highlight::highlight_lines(h, &lines)).unwrap_or_default();
    Ok(Some(Content::Text { lines, spans, truncated }))
}

/// A worker that loads the latest requested preview, skipping requests that
/// were superseded while it was busy.
struct Loader {
    tx: Sender<PathBuf>,
    done: Arc<Mutex<Option<Preview>>>,
    thread: std::thread::JoinHandle<()>,
}

impl Loader {
    fn new() -> Self {
        Self::with(load)
    }

    /// A loader that runs `load`. A panic in it (a decoder bug on a malformed
    /// file) becomes that entry's error rather than ending the worker.
    fn with(load: fn(&Path) -> Preview) -> Self {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let done = Arc::new(Mutex::new(None));
        let out = done.clone();
        let thread = std::thread::Builder::new()
            .name("preview-loader".into())
            .spawn(move || {
                while let Ok(mut path) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        path = newer;
                    }
                    let p = std::panic::catch_unwind(|| load(&path)).unwrap_or_else(|_| Preview {
                        path: path.clone(),
                        kind: "File".into(),
                        size: None,
                        modified: None,
                        content: Content::Error("can't preview this file".into()),
                    });
                    if let Ok(mut slot) = out.lock() {
                        *slot = Some(p);
                    }
                }
            })
            .expect("spawn preview loader");
        Loader { tx, done, thread }
    }

    fn take(&self) -> Option<Preview> {
        self.done.lock().ok()?.take()
    }

    fn alive(&self) -> bool {
        !self.thread.is_finished()
    }
}

/// The preview pane's state, owned by a file browser view.
pub struct PreviewPane {
    /// Started on the first preview.
    loader: Option<Loader>,
    /// The entry to show (the last one asked for).
    wanted: Option<PathBuf>,
    /// What is drawn: `wanted`'s preview once loaded, the previous one until then.
    shown: Option<Preview>,
    /// When to next check for `wanted`'s preview (while loading).
    poll_at: Option<Instant>,
    /// When autoplay starts the shown audio.
    autoplay_at: Option<Instant>,
    /// The view is off screen: nothing plays and nothing autoplays.
    hidden: bool,
    /// When to redraw the playback position.
    progress_at: Option<Instant>,
    /// This pane's autoplay override; `None` follows `[file_browser] preview_autoplay`.
    autoplay: Option<bool>,
    /// Started on the first play.
    audio: Option<AudioPlayer>,
    /// Text scroll (physical px).
    scroll: f32,
    // Geometry from the last paint, for input.
    rect: Rect,
    /// The seek bar's hit rect and its track's x span.
    seek: Option<(Rect, f32, f32)>,
}

impl Default for PreviewPane {
    fn default() -> Self {
        PreviewPane {
            loader: None,
            wanted: None,
            shown: None,
            poll_at: None,
            autoplay_at: None,
            hidden: false,
            progress_at: None,
            autoplay: None,
            audio: None,
            scroll: 0.0,
            rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            seek: None,
        }
    }
}

impl PreviewPane {
    /// Show `path` (`None`: nothing). Loads it unless it is already wanted;
    /// moving off a playing track pauses it.
    pub fn show(&mut self, path: Option<&Path>) {
        if self.wanted.as_deref() == path {
            return;
        }
        self.wanted = path.map(Path::to_path_buf);
        self.scroll = 0.0;
        self.autoplay_at = None;
        if let Some(a) = &self.audio {
            let st = a.status();
            if st.playing && st.path.as_deref() != path {
                a.pause();
            }
        }
        match path {
            Some(p) => {
                let loader = self.loader.get_or_insert_with(Loader::new);
                let _ = loader.tx.send(p.to_path_buf());
                self.poll_at = Some(Instant::now() + POLL_EVERY);
            }
            None => {
                self.shown = None;
                self.poll_at = None;
            }
        }
    }

    /// The view came on screen or left it. Leaving pauses a playing track and
    /// cancels a pending autoplay; coming back resumes nothing, so a track
    /// never starts without the user seeing the view.
    pub fn set_visible(&mut self, visible: bool) {
        self.hidden = !visible;
        if visible {
            return;
        }
        self.autoplay_at = None;
        self.progress_at = None;
        if let Some(a) = self.audio.as_ref().filter(|a| a.status().playing) {
            a.pause();
        }
    }

    /// Use `player` for playback.
    #[cfg(test)]
    pub fn install_player(&mut self, player: AudioPlayer) {
        self.audio = Some(player);
    }

    /// The audio player's state, once one exists.
    #[cfg(test)]
    pub fn audio_status(&self) -> Option<audio::Status> {
        self.audio.as_ref().map(AudioPlayer::status)
    }

    /// The preview drawn now.
    #[cfg(test)]
    pub fn shown(&self) -> Option<&Preview> {
        self.shown.as_ref()
    }

    /// Whether the wanted preview is still loading.
    pub fn loading(&self) -> bool {
        self.poll_at.is_some()
    }

    fn autoplay_on(&self, cfg: &Config) -> bool {
        self.autoplay.unwrap_or(cfg.file_browser.preview_autoplay)
    }

    pub fn deadline(&self) -> Option<Instant> {
        [self.poll_at, self.autoplay_at, self.progress_at].into_iter().flatten().min()
    }

    pub fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        if self.poll_at.is_some_and(|t| now >= t) {
            self.poll_at = Some(now + POLL_EVERY);
            let ready = self.loader.as_ref().and_then(Loader::take);
            if let Some(p) = ready.filter(|p| Some(&p.path) == self.wanted.as_ref()) {
                if matches!(p.content, Content::Audio { .. }) && self.autoplay_on(cx.cfg) && !self.hidden {
                    self.autoplay_at = Some(now + AUTOPLAY_AFTER);
                }
                self.shown = Some(p);
                self.poll_at = None;
                cx.redraw();
            } else if self.loader.as_ref().is_some_and(|l| !l.alive()) {
                // Nothing more will arrive: stop polling, and start a fresh
                // loader for the next entry.
                self.loader = None;
                self.poll_at = None;
                self.shown = self.wanted.clone().map(|path| Preview {
                    path,
                    kind: "File".into(),
                    size: None,
                    modified: None,
                    content: Content::Error("the preview stopped".into()),
                });
                cx.redraw();
            }
        }
        if self.autoplay_at.is_some_and(|t| now >= t) {
            self.autoplay_at = None;
            if let Some(p) = self.audio_path() {
                self.audio.get_or_insert_with(AudioPlayer::new).play(&p);
                cx.redraw();
            }
        }
        if self.progress_at.is_some_and(|t| now >= t) {
            self.progress_at = None;
            cx.redraw();
        }
    }

    /// The shown entry, if it is audio.
    fn audio_path(&self) -> Option<PathBuf> {
        self.shown
            .as_ref()
            .filter(|p| matches!(p.content, Content::Audio { .. }))
            .map(|p| p.path.clone())
    }

    /// Whether `pos` is over the pane (as last painted).
    pub fn contains(&self, pos: (f32, f32)) -> bool {
        rect_contains(self.rect, pos.0, pos.1)
    }

    pub fn scroll(&mut self, cx: &mut EventCx, dy: f32) {
        let next = (self.scroll - dy).max(0.0);
        if next != self.scroll {
            self.scroll = next;
            cx.request_frame();
        }
    }

    /// A press in the pane: seeks when it lands on the seek bar.
    pub fn mouse_down(&mut self, cx: &mut EventCx, pos: (f32, f32)) {
        let Some((hit, x0, x1)) = self.seek else { return };
        if !rect_contains(hit, pos.0, pos.1) {
            return;
        }
        let Some(Content::Audio { duration: Some(total) }) = self.shown.as_ref().map(|p| &p.content) else {
            return;
        };
        let frac = ((pos.0 - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0);
        let at = total.mul_f32(frac);
        if let (Some(a), Some(path)) = (&self.audio, self.audio_path()) {
            if a.status().path.as_ref() == Some(&path) {
                a.seek(at);
                cx.redraw();
            }
        }
    }

    /// Returns whether `id` was one of the pane's buttons.
    pub fn button(&mut self, cx: &mut EventCx, id: u32) -> bool {
        match id {
            BTN_PLAY => {
                let Some(path) = self.audio_path() else { return true };
                self.autoplay_at = None;
                let player = self.audio.get_or_insert_with(AudioPlayer::new);
                let st = player.status();
                if st.path.as_ref() != Some(&path) || !st.loaded {
                    player.play(&path);
                } else if st.playing {
                    player.pause();
                } else {
                    player.resume();
                }
            }
            BTN_AUTOPLAY => self.autoplay = Some(!self.autoplay_on(cx.cfg)),
            _ => return false,
        }
        cx.redraw();
        true
    }

    pub fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        self.rect = rect;
        self.seek = None;
        let scale = cx.ui.scale;
        let ch = cx.ui.cell_h;
        let pad = 12.0 * scale;
        cx.fill(rect, cx.chrome.sidebar);
        cx.fill(Rect { x: rect.x, y: rect.y, w: scale.max(1.0), h: rect.h }, theme::INPUT_BORDER);
        let inner = Rect {
            x: rect.x + pad,
            y: rect.y + pad,
            w: (rect.w - pad * 2.0).max(1.0),
            h: (rect.h - pad * 2.0).max(1.0),
        };
        let Some(p) = self.shown.clone() else {
            let msg = if self.loading() { "Loading\u{2026}" } else { "Nothing selected" };
            cx.label(msg, Family::SansSerif, inner.x, inner.y, inner, theme::DIM);
            return;
        };

        // Details: name, then kind/size/dimensions, then age.
        let mut y = inner.y;
        let name = p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        cx.label(&name, Font::SANS.bold(true), inner.x, y, inner, theme::TEXT);
        y += ch * 1.4;
        let mut facts = vec![p.kind.clone()];
        if let Some(size) = p.size {
            facts.push(human_size(size));
        }
        match &p.content {
            Content::Dir { entries } => facts.push(match entries {
                1 => "1 item".to_string(),
                n => format!("{n} items"),
            }),
            Content::Image { width, height, .. } => facts.push(format!("{width} \u{00d7} {height}")),
            Content::Audio { duration: Some(d) } => facts.push(clock(*d)),
            _ => {}
        }
        cx.label(&facts.join("  \u{00b7}  "), Family::SansSerif, inner.x, y, inner, theme::TEXT_DIM);
        y += ch * 1.2;
        if let Some(m) = p.modified {
            cx.label(&format!("Modified {}", ago(m)), Family::SansSerif, inner.x, y, inner, theme::TEXT_DIM);
            y += ch * 1.2;
        }
        y += pad * 0.5;
        let body = Rect { x: inner.x, y, w: inner.w, h: (inner.y + inner.h - y).max(0.0) };
        if body.h < ch {
            return;
        }

        match &p.content {
            Content::Text { lines, spans, truncated } => self.paint_text(cx, body, lines, spans, *truncated),
            Content::Image { image, .. } => {
                // One image pixel per logical pixel at most: small images stay crisp.
                let r = image.fit(body, scale);
                cx.image(IMAGE_ID, image.id(), image.clone(), r, body);
            }
            Content::Audio { duration } => self.paint_audio(cx, body, &p.path, *duration),
            Content::Nothing => {
                cx.label("No preview", Family::SansSerif, body.x, body.y, body, theme::DIM);
            }
            Content::Error(e) => {
                cx.label(e, Family::SansSerif, body.x, body.y, body, theme::DIM);
            }
            Content::Dir { .. } => {}
        }
    }

    fn paint_text(&mut self, cx: &mut PaintCx, body: Rect, lines: &[String], spans: &[Vec<Span>], truncated: bool) {
        let ch = cx.ui.cell_h;
        let count = lines.len() + usize::from(truncated);
        if count == 0 {
            cx.label("Empty file", Family::SansSerif, body.x, body.y, body, theme::DIM);
            return;
        }
        let max_scroll = (count as f32 * ch - body.h).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let first = (self.scroll / ch).floor() as usize;
        let mut i = first;
        while i < count {
            let top = body.y + i as f32 * ch - self.scroll;
            if top > body.y + body.h {
                break;
            }
            let (text, color) = match lines.get(i) {
                Some(l) => (l.as_str(), theme::TEXT),
                None => ("\u{2026}", theme::DIM),
            };
            match spans.get(i) {
                Some(hl) => {
                    let row = syntax::row_spans(text, hl, 0, usize::MAX, color);
                    let key = cx.row_key(row.iter().map(|(t, c)| (t.as_str(), *c)));
                    cx.budgeted_row(key, || row, body.x, top, body.w, body, color);
                }
                None => {
                    let key = cx.row_key(std::iter::once((text, color)));
                    cx.budgeted_row(key, || vec![(text.to_string(), color)], body.x, top, body.w, body, color);
                }
            }
            i += 1;
        }
    }

    fn paint_audio(&mut self, cx: &mut PaintCx, body: Rect, path: &Path, duration: Option<Duration>) {
        let scale = cx.ui.scale;
        let (cw, ch) = (cx.ui.cell_w, cx.ui.cell_h);
        let pad = 8.0 * scale;
        let st = self
            .audio
            .as_ref()
            .map(AudioPlayer::status)
            .filter(|s| s.path.as_deref() == Some(path))
            .unwrap_or_default();
        if st.playing {
            self.progress_at.get_or_insert(Instant::now() + PROGRESS_EVERY);
        }
        let total = st.total.or(duration);

        // [Play|Pause|Resume]  0:12 / 3:45                       autoplay
        let bh = ch + 8.0 * scale;
        let label = match (st.loaded, st.playing) {
            (true, true) => "Pause",
            (true, false) => "Resume",
            _ => "Play",
        };
        let bw = label.chars().count() as f32 * cw + pad * 2.0;
        let btn = Rect { x: body.x, y: body.y, w: bw, h: bh };
        let hov = cx.button(btn, BTN_PLAY);
        cx.fill_alpha(btn, cx.chrome.accent, if hov { 0.6 } else { 0.35 });
        let ty = btn.y + (bh - ch) * 0.5;
        cx.label(label, Family::SansSerif, btn.x + pad, ty, btn, theme::TEXT);
        let time = match total {
            Some(t) => format!("{} / {}", clock(st.pos), clock(t)),
            None => clock(st.pos),
        };
        cx.label(&time, Family::Monospace, btn.x + bw + pad, ty, body, theme::TEXT_DIM);

        let auto = self.autoplay_on(cx.cfg);
        let aw = "autoplay".len() as f32 * cw;
        let ahit = Rect { x: body.x + body.w - aw - pad * 2.0, y: body.y, w: aw + pad * 2.0, h: bh };
        if cx.button(ahit, BTN_AUTOPLAY) {
            cx.highlight(ahit);
        }
        let acolor = if auto { theme::LABEL_HOVER } else { theme::DIM };
        cx.label("autoplay", Family::SansSerif, ahit.x + pad, ty, ahit, acolor);

        // The seek bar: a track, filled to the position.
        let Some(total) = total.filter(|t| !t.is_zero()) else { return };
        let track_h = 4.0 * scale;
        let hit = Rect { x: body.x, y: body.y + bh + pad * 0.5, w: body.w, h: ch };
        let track = Rect { x: hit.x, y: hit.y + (hit.h - track_h) * 0.5, w: hit.w, h: track_h };
        cx.fill(track, theme::INPUT_BORDER);
        let frac = (st.pos.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0);
        cx.fill(Rect { w: track.w * frac, ..track }, cx.chrome.accent);
        self.seek = Some((hit, track.x, track.x + track.w));
        if let Some(e) = &st.error {
            cx.label(e, Family::SansSerif, body.x, hit.y + hit.h + pad, body, theme::DIM);
        }
    }
}

/// A byte count the way Finder shows it (powers of 1000).
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1000.0;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

/// `m:ss`, or `h:mm:ss` from an hour.
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// How long ago `t` was, in its largest whole unit ("3 days ago").
pub fn ago(t: SystemTime) -> String {
    let Ok(d) = SystemTime::now().duration_since(t) else {
        return "just now".to_string();
    };
    let s = d.as_secs();
    let (n, unit) = match s {
        0..60 => return "just now".to_string(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        86_400..2_592_000 => (s / 86_400, "day"),
        2_592_000..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::testing::wav;
    use crate::plugin::testing::tmpdir;

    #[test]
    fn text_keeps_a_bounded_head_and_spots_binary() {
        let d = tmpdir("preview-text");
        std::fs::write(d.join("a.rs"), "fn main() {\n\tx\n}\n").unwrap();
        let p = load(&d.join("a.rs"));
        assert_eq!(p.kind, "RS text");
        let Content::Text { lines, truncated, .. } = p.content else { panic!("text") };
        assert_eq!(lines, vec!["fn main() {", "    x", "}"]);
        assert!(!truncated);

        std::fs::write(d.join("long.txt"), "x\n".repeat(TEXT_MAX_LINES + 5)).unwrap();
        let Content::Text { lines, truncated, .. } = load(&d.join("long.txt")).content else { panic!("text") };
        assert_eq!(lines.len(), TEXT_MAX_LINES);
        assert!(truncated);

        std::fs::write(d.join("blob.bin"), [0x7f, b'E', b'L', b'F', 0, 1]).unwrap();
        let p = load(&d.join("blob.bin"));
        assert!(matches!(p.content, Content::Nothing));
        assert_eq!(p.kind, "BIN file");
        assert_eq!(p.size, Some(6));
    }

    #[test]
    fn text_with_a_highlighter_carries_a_span_list_per_line() {
        use ghostrealm_core::highlight::Token;
        let d = tmpdir("preview-highlight");
        std::fs::write(d.join("n.md"), "# Title\n```\n# code\n```\n").unwrap();
        let Content::Text { lines, spans, .. } = load(&d.join("n.md")).content else { panic!("text") };
        assert_eq!(spans.len(), lines.len());
        assert_eq!(spans[0][0].token, Token::Marker, "heading");
        assert_eq!(spans[2][0].token, Token::Code, "inside the fence");

        std::fs::write(d.join("a.rs"), "fn main() {}\n").unwrap();
        let Content::Text { spans, .. } = load(&d.join("a.rs")).content else { panic!("text") };
        assert!(spans.is_empty(), "no highlighter for .rs");
    }

    #[test]
    fn folders_count_their_entries() {
        let d = tmpdir("preview-dir");
        std::fs::write(d.join("a"), "").unwrap();
        std::fs::write(d.join("b"), "").unwrap();
        let p = load(&d);
        assert_eq!(p.kind, "Folder");
        assert!(matches!(p.content, Content::Dir { entries: 2 }));
    }

    #[test]
    fn images_decode_and_shrink_to_the_cap() {
        let d = tmpdir("preview-img");
        std::fs::write(d.join("wide.png"), media::testing::png(IMAGE_MAX_SIDE * 2, 10, [255, 0, 0, 255])).unwrap();
        let p = load(&d.join("wide.png"));
        assert_eq!(p.kind, "PNG image");
        let Content::Image { image, width, height } = p.content else { panic!("image") };
        assert_eq!((width, height), (IMAGE_MAX_SIDE * 2, 10), "reports the file's size");
        assert_eq!(image.width(), IMAGE_MAX_SIDE, "decoded at most the cap");

        std::fs::write(d.join("bad.png"), "not a png").unwrap();
        assert!(matches!(load(&d.join("bad.png")).content, Content::Error(_)));
    }

    #[cfg(unix)]
    #[test]
    fn pipes_and_devices_are_described_without_being_read() {
        let d = tmpdir("preview-fifo");
        // Named like audio, so the extension doesn't pick the plain-file path.
        let fifo = d.join("pipe.mp3");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let (tx, rx) = mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(load(&path));
        });
        let p = rx.recv_timeout(Duration::from_secs(5)).expect("loading a FIFO hung");
        assert_eq!(p.kind, "Pipe");
        assert!(matches!(p.content, Content::Nothing));

        let p = load(Path::new("/dev/null"));
        assert_eq!(p.kind, "Device");
        assert!(matches!(p.content, Content::Nothing));
    }

    #[test]
    fn an_image_with_no_pixels_is_an_error_not_a_panic() {
        let d = tmpdir("preview-empty-img");
        // A fuzzed 17x0 GIF that used to panic building mips.
        std::fs::write(d.join("empty.gif"), include_bytes!("../../media/testdata/zero-height.gif")).unwrap();
        let p = load(&d.join("empty.gif"));
        assert!(matches!(p.content, Content::Error(_)), "{:?}", p.kind);
    }

    fn tick(pane: &mut PreviewPane, now: Instant) {
        let cfg = Config::default();
        let mut cx = EventCx::new(&cfg, crate::plugin::testing::UI, Default::default(), None);
        pane.tick(&mut cx, now);
    }

    /// Tick `pane` until it stops polling (or a second passes).
    fn settle(pane: &mut PreviewPane) {
        let until = Instant::now() + Duration::from_secs(1);
        while pane.loading() && Instant::now() < until {
            std::thread::sleep(POLL_EVERY);
            tick(pane, Instant::now());
        }
    }

    #[test]
    fn a_panicking_load_shows_an_error_and_the_loader_carries_on() {
        fn explode(path: &Path) -> Preview {
            if path.ends_with("bad") {
                panic!("decoder bug");
            }
            load(path)
        }
        let d = tmpdir("preview-panic");
        std::fs::write(d.join("good.txt"), "hi").unwrap();
        let mut pane = PreviewPane { loader: Some(Loader::with(explode)), ..Default::default() };
        pane.show(Some(&d.join("bad")));
        settle(&mut pane);
        assert!(!pane.loading());
        assert!(matches!(pane.shown().unwrap().content, Content::Error(_)));
        pane.show(Some(&d.join("good.txt")));
        settle(&mut pane);
        assert!(matches!(pane.shown().unwrap().content, Content::Text { .. }));
    }

    #[test]
    fn a_dead_loader_stops_the_polling() {
        let d = tmpdir("preview-dead");
        std::fs::write(d.join("a.txt"), "a").unwrap();
        let mut pane = PreviewPane::default();
        pane.show(Some(&d.join("a.txt")));
        settle(&mut pane);
        // A loader whose worker has gone, with a request it will never answer.
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || drop(rx));
        thread_wait(&thread);
        pane.loader = Some(Loader { tx, done: Arc::default(), thread });
        pane.show(Some(&d.join("gone")));
        settle(&mut pane);
        assert!(!pane.loading(), "stops polling");
        assert!(pane.deadline().is_none(), "no timer left");
        assert!(matches!(pane.shown().unwrap().content, Content::Error(_)));
        assert!(pane.loader.is_none(), "the next entry starts a new loader");
    }

    fn thread_wait(t: &std::thread::JoinHandle<()>) {
        while !t.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn audio_reports_its_length() {
        let d = tmpdir("preview-wav");
        std::fs::write(d.join("tone.wav"), wav(8000, 8000 * 2)).unwrap();
        let p = load(&d.join("tone.wav"));
        assert_eq!(p.kind, "WAV audio");
        let Content::Audio { duration } = p.content else { panic!("audio") };
        assert_eq!(duration, Some(Duration::from_secs(2)));
    }

    #[test]
    fn formats_sizes_clocks_and_ages() {
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_234), "1.2 KB");
        assert_eq!(human_size(45_600_000), "46 MB");
        assert_eq!(clock(Duration::from_secs(65)), "1:05");
        assert_eq!(clock(Duration::from_secs(3_725)), "1:02:05");
        let now = SystemTime::now();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - Duration::from_secs(3 * 86_400 + 5)), "3 days ago");
        assert_eq!(ago(now - Duration::from_secs(3_700)), "1 hour ago");
    }
}
