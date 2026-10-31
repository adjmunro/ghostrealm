//! [`MediaView`]: a Files surface document showing an image or playing a
//! track, under its [`DocHeader`]. Decoding runs on a worker; the view polls
//! for the result on a timer.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ghostrealm_core::{Config, HeaderIcon, Rect};
use glyphon::Family;

use super::header::{DocHeader, HeaderText};
use crate::file_menu::{FileTarget, Kind as TargetKind, Origin};
use crate::media::audio::{self, AudioPlayer};
use crate::media;
use crate::plugin::icons::Icon;
use crate::plugin::{
    hover_box, rect_contains, theme, EventCx, Image, MenuAlign, MenuItem, MouseEvent, PaintCx, Request, View,
};

/// Button and menu item ids (see `View::button`).
pub const BTN_PLAY: u32 = 0;
pub const BTN_AUTOPLAY: u32 = 1;
const BTN_NAME: u32 = 2;
const BTN_MORE: u32 = 3;
const BTN_FILE_MENU: u32 = 4;
/// The image within the view (see `PaintCx::image`).
const IMAGE_ID: u32 = 0;

/// Images larger than this on either side are downscaled when decoded.
const IMAGE_MAX_SIDE: u32 = 2048;
/// How often to check for a pending load's result.
const POLL_EVERY: Duration = Duration::from_millis(15);
/// How long a track must stay shown before autoplay starts it (so a quick
/// look stays quiet).
const AUTOPLAY_AFTER: Duration = Duration::from_millis(400);
/// How often the playback position redraws.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

const IMAGE_EXTS: &[&str] = &["bmp", "gif", "ico", "jpeg", "jpg", "png", "tif", "tiff", "webp"];
const AUDIO_EXTS: &[&str] = &["flac", "m4a", "mp3", "oga", "ogg", "wav"];

/// Whether `path` names an image or a track (by extension), which a
/// [`MediaView`] shows rather than the editor.
pub fn is_media(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    IMAGE_EXTS.contains(&ext.as_str()) || AUDIO_EXTS.contains(&ext.as_str())
}

/// What a loaded file shows.
#[derive(Clone)]
pub enum Content {
    /// `width` x `height` is the file's size; `image` may be downscaled.
    Image { image: Arc<Image>, width: u32, height: u32 },
    Audio { duration: Option<Duration> },
    /// A pipe, socket, or device: described, never read.
    Nothing,
    Error(String),
}

/// A file's loaded media.
#[derive(Clone)]
pub struct Loaded {
    pub path: PathBuf,
    pub kind: String,
    pub content: Content,
}

/// Read and decode `path`. Runs on the loader thread.
pub fn load(path: &Path) -> Loaded {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let upper = ext.to_uppercase();
    let loaded = |kind: String, content: Content| Loaded { path: path.to_path_buf(), kind, content };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return loaded("Missing".into(), Content::Error(e.to_string())),
    };
    if !meta.is_file() {
        // Opening a pipe or device can block forever (and reading eats a
        // pipe's data): describe it without touching its content.
        return loaded(special_kind(&meta).into(), Content::Nothing);
    }
    if AUDIO_EXTS.contains(&ext.as_str()) {
        let content = match audio::duration(path) {
            Ok(duration) => Content::Audio { duration },
            Err(e) => Content::Error(e),
        };
        return loaded(format!("{upper} audio"), content);
    }
    let content = match decode_image(path) {
        Ok(c) => c,
        Err(e) => Content::Error(e),
    };
    loaded(format!("{upper} image"), content)
}

/// What a non-regular entry is.
fn special_kind(meta: &std::fs::Metadata) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let t = meta.file_type();
        if t.is_dir() {
            return "Folder";
        }
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

/// A worker that loads the latest requested file, skipping requests that
/// were superseded while it was busy.
struct Loader {
    tx: Sender<PathBuf>,
    done: Arc<Mutex<Option<Loaded>>>,
    thread: std::thread::JoinHandle<()>,
}

impl Loader {
    fn new() -> Self {
        Self::with(load)
    }

    /// A loader that runs `load`. A panic in it (a decoder bug on a malformed
    /// file) becomes that file's error rather than ending the worker.
    fn with(load: fn(&Path) -> Loaded) -> Self {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let done = Arc::new(Mutex::new(None));
        let out = done.clone();
        let thread = std::thread::Builder::new()
            .name("media-loader".into())
            .spawn(move || {
                while let Ok(mut path) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        path = newer;
                    }
                    let p = std::panic::catch_unwind(|| load(&path)).unwrap_or_else(|_| Loaded {
                        path: path.clone(),
                        kind: "File".into(),
                        content: Content::Error("can't show this file".into()),
                    });
                    if let Ok(mut slot) = out.lock() {
                        *slot = Some(p);
                    }
                }
            })
            .expect("spawn media loader");
        Loader { tx, done, thread }
    }

    fn take(&self) -> Option<Loaded> {
        self.done.lock().ok()?.take()
    }

    fn alive(&self) -> bool {
        !self.thread.is_finished()
    }
}

/// An image or a track, as a document.
pub struct MediaView {
    path: PathBuf,
    header: DocHeader,
    /// Started on the first load.
    loader: Option<Loader>,
    /// What is drawn: the file once loaded (the previous load until a reload
    /// is ready).
    shown: Option<Loaded>,
    /// When to next check for the load's result (while loading).
    poll_at: Option<Instant>,
    /// When autoplay starts the track.
    autoplay_at: Option<Instant>,
    /// The view is off screen: nothing plays and nothing autoplays.
    hidden: bool,
    /// When to redraw the playback position.
    progress_at: Option<Instant>,
    /// Started on the first play.
    audio: Option<AudioPlayer>,
    // Geometry from the last paint, for input.
    header_rect: Option<Rect>,
    name_hit: Option<Rect>,
    more_hit: Option<Rect>,
    /// The seek bar's hit rect and its track's x span.
    seek: Option<(Rect, f32, f32)>,
}

impl MediaView {
    /// A view of `path`, loading it.
    pub fn open(path: PathBuf) -> Self {
        let mut v = MediaView {
            path,
            header: DocHeader::default(),
            loader: None,
            shown: None,
            poll_at: None,
            autoplay_at: None,
            hidden: true,
            progress_at: None,
            audio: None,
            header_rect: None,
            name_hit: None,
            more_hit: None,
            seek: None,
        };
        v.load();
        v
    }

    /// (Re)load the file on the worker.
    fn load(&mut self) {
        self.header.refresh(Some(&self.path));
        let loader = self.loader.get_or_insert_with(Loader::new);
        let _ = loader.tx.send(self.path.clone());
        self.poll_at = Some(Instant::now() + POLL_EVERY);
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

    /// The file as loaded.
    #[cfg(test)]
    pub fn loaded(&self) -> Option<&Loaded> {
        self.shown.as_ref()
    }

    /// Whether the load is still running.
    pub fn loading(&self) -> bool {
        self.poll_at.is_some()
    }

    fn autoplay_on(&self, cfg: &Config) -> bool {
        cfg.file_browser.preview_autoplay
    }

    /// The file, if it is a track.
    fn audio_path(&self) -> Option<PathBuf> {
        self.shown
            .as_ref()
            .filter(|p| matches!(p.content, Content::Audio { .. }))
            .map(|p| p.path.clone())
    }

    /// What the file is, from its extension until it has loaded.
    fn kind(&self) -> String {
        if let Some(p) = &self.shown {
            return p.kind.clone();
        }
        let ext = self.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let what = if AUDIO_EXTS.contains(&ext.as_str()) { "audio" } else { "image" };
        format!("{} {what}", ext.to_uppercase())
    }

    /// The facts after the kind and size: an image's size, a track's length.
    fn extra(&self) -> Option<String> {
        match &self.shown.as_ref()?.content {
            Content::Image { width, height, .. } => Some(format!("{width} \u{00d7} {height}")),
            Content::Audio { duration: Some(d) } => Some(clock(*d)),
            _ => None,
        }
    }

    fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// The `⋯` menu: autoplay for a track, and the file menu.
    fn more_menu(&self, cfg: &Config) -> Vec<MenuItem> {
        let mut items = Vec::new();
        if self.audio_path().is_some() {
            items.push(MenuItem::new(BTN_AUTOPLAY, "Autoplay").check(self.autoplay_on(cfg)));
            items.push(MenuItem::RULE);
        }
        items.push(MenuItem::new(BTN_FILE_MENU, "File actions\u{2026}"));
        items
    }

    fn open_file_menu(&self, cx: &mut EventCx, at: (f32, f32), align: MenuAlign) {
        let target = FileTarget { path: self.path.clone(), kind: TargetKind::File, origin: Origin::Document };
        cx.request(Request::FileMenu { target, at, align });
    }

    /// The header with `⋯` at its right end and, for a track that
    /// `[editor] header_icons` lists it for, the autoplay toggle left of it.
    /// Returns the header's height.
    fn paint_header(&mut self, cx: &mut PaintCx, rect: Rect) -> f32 {
        let h = DocHeader::height(cx.ui);
        let r = Rect { h, ..rect };
        let row = DocHeader::control_row(cx.ui, r);
        let side = row.h;
        let mb = Rect { x: row.x + row.w - side, y: row.y, w: side, h: side };
        let listed = cx.cfg.editor.header_icons.contains(&HeaderIcon::Autoplay);
        let ab = (listed && self.audio_path().is_some()).then_some(Rect { x: mb.x - side, ..mb });
        // The header's background goes first: `⋯` draws over it.
        let (kind, extra, name) = (self.kind(), self.extra(), self.name());
        let text = HeaderText {
            name: &name,
            path: Some(&self.path),
            kind: &kind,
            modified: false,
            changed_on_disk: false,
            extra,
        };
        self.name_hit = Some(self.header.paint(cx, r, ab.unwrap_or(mb).x, &text, Some(BTN_NAME)));
        let radius = theme::box_radius(cx.ui);
        let icon_side = (cx.ui.cell_h * 0.8).round();
        let icon_in = |b: Rect| Rect {
            x: b.x + ((b.w - icon_side) * 0.5).round(),
            y: b.y + ((b.h - icon_side) * 0.5).round(),
            w: icon_side,
            h: icon_side,
        };
        let hover = cx.button(mb, BTN_MORE);
        if hover {
            cx.round_rect(hover_box(mb, side), radius, theme::HOVER_BG, theme::HOVER_ALPHA, None);
        }
        cx.tooltip(mb, "More options");
        Icon::More.paint(cx, icon_in(mb), if hover { theme::GLYPH_HOVER } else { theme::GLYPH });
        self.more_hit = Some(mb);
        if let Some(ab) = ab {
            if cx.button(ab, BTN_AUTOPLAY) {
                cx.round_rect(hover_box(ab, side), radius, theme::HOVER_BG, theme::HOVER_ALPHA, None);
            }
            cx.tooltip(ab, "Autoplay");
            let color = if self.autoplay_on(cx.cfg) { cx.chrome.accent } else { theme::GLYPH };
            Icon::Play.paint(cx, icon_in(ab), color);
        }
        self.header_rect = Some(r);
        h
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

impl View for MediaView {
    fn plugin(&self) -> &'static str {
        super::ID
    }

    fn title(&self) -> Option<String> {
        Some(self.name())
    }

    fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        self.seek = None;
        cx.fill(rect, cx.chrome.background);
        let header_h = self.paint_header(cx, rect);
        let pad = (12.0 * cx.ui.scale).round();
        let body = Rect {
            x: rect.x + pad,
            y: rect.y + header_h + pad,
            w: (rect.w - pad * 2.0).max(1.0),
            h: (rect.h - header_h - pad * 2.0).max(0.0),
        };
        let Some(p) = self.shown.clone() else {
            cx.label("Loading\u{2026}", Family::SansSerif, body.x, body.y, body, theme::DIM);
            return;
        };
        match &p.content {
            Content::Image { image, .. } => {
                // One image pixel per logical pixel at most: small images stay crisp.
                let r = image.fit(body, cx.ui.scale);
                cx.image(IMAGE_ID, image.id(), image.clone(), r, body);
            }
            Content::Audio { duration } => self.paint_audio(cx, body, &p.path, *duration),
            Content::Nothing => {
                cx.label("Nothing to show", Family::SansSerif, body.x, body.y, body, theme::DIM);
            }
            Content::Error(e) => {
                cx.label(e, Family::SansSerif, body.x, body.y, body, theme::DIM);
            }
        }
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        let MouseEvent::Down { pos, button: crate::plugin::MouseButton::Left, .. } = *event else { return };
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

    fn context_menu(&mut self, cx: &mut EventCx, pos: (f32, f32)) {
        if self.header_rect.is_some_and(|r| rect_contains(r, pos.0, pos.1)) {
            self.open_file_menu(cx, pos, MenuAlign::Left);
        }
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        match id {
            BTN_PLAY => {
                let Some(path) = self.audio_path() else { return };
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
            BTN_AUTOPLAY => cx.request(Request::SetConfig {
                section: "file_browser",
                key: "preview_autoplay",
                value: toml::Value::Boolean(!self.autoplay_on(cx.cfg)),
            }),
            BTN_NAME => {
                if let Some(hit) = self.name_hit {
                    self.open_file_menu(cx, (hit.x, hit.y + hit.h), MenuAlign::Left);
                }
                return;
            }
            BTN_MORE => {
                if let Some(mb) = self.more_hit {
                    let items = self.more_menu(cx.cfg);
                    cx.request(Request::Menu { items, at: (mb.x + mb.w, mb.y + mb.h), align: MenuAlign::Right });
                }
                return;
            }
            BTN_FILE_MENU => {
                if let Some(mb) = self.more_hit {
                    self.open_file_menu(cx, (mb.x + mb.w, mb.y + mb.h), MenuAlign::Right);
                }
                return;
            }
            _ => return,
        }
        cx.redraw();
    }

    /// Leaving the screen pauses a playing track and cancels a pending
    /// autoplay; coming back resumes nothing, so a track never starts without
    /// the user seeing it.
    fn shown(&mut self, visible: bool) {
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

    fn deadline(&self, _cfg: &Config) -> Option<Instant> {
        [self.poll_at, self.autoplay_at, self.progress_at].into_iter().flatten().min()
    }

    fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        if self.poll_at.is_some_and(|t| now >= t) {
            self.poll_at = Some(now + POLL_EVERY);
            let ready = self.loader.as_ref().and_then(Loader::take);
            if let Some(p) = ready.filter(|p| p.path == self.path) {
                let first = self.shown.is_none();
                if first && matches!(p.content, Content::Audio { .. }) && self.autoplay_on(cx.cfg) && !self.hidden {
                    self.autoplay_at = Some(now + AUTOPLAY_AFTER);
                }
                self.shown = Some(p);
                self.poll_at = None;
                cx.redraw();
            } else if self.loader.as_ref().is_some_and(|l| !l.alive()) {
                // Nothing more will arrive: stop polling, and start a fresh
                // loader for the next load.
                self.loader = None;
                self.poll_at = None;
                self.shown = Some(Loaded {
                    path: self.path.clone(),
                    kind: "File".into(),
                    content: Content::Error("loading stopped".into()),
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

    fn path(&self) -> Option<PathBuf> {
        Some(self.path.clone())
    }

    /// Copies the file as it is on disk: the view never changes it.
    fn save_copy(&mut self, path: PathBuf) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(&self.path, &path)?;
        Ok(())
    }

    fn files_changed(&mut self, moved: &[(PathBuf, PathBuf)]) {
        if let Some(to) = crate::file_ops::follow(&self.path, moved) {
            self.path = to;
            self.load();
        }
    }

    fn changed_on_disk(&mut self, path: &Path) -> bool {
        if !crate::languages::same_file(&self.path, path) {
            return false;
        }
        self.load();
        true
    }

    fn text(&mut self) -> Option<String> {
        let mut facts = vec![self.kind()];
        facts.extend(self.extra());
        Some(format!("{}\n{}", self.path.display(), facts.join(" \u{00b7} ")))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::testing::wav;
    use crate::plugin::testing::{tmpdir, Harness};

    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 600.0, h: 400.0 };

    #[test]
    fn images_decode_and_shrink_to_the_cap() {
        let d = tmpdir("media-img");
        std::fs::write(d.join("wide.png"), media::testing::png(IMAGE_MAX_SIDE * 2, 10, [255, 0, 0, 255])).unwrap();
        let p = load(&d.join("wide.png"));
        assert_eq!(p.kind, "PNG image");
        let Content::Image { image, width, height } = p.content else { panic!("image") };
        assert_eq!((width, height), (IMAGE_MAX_SIDE * 2, 10), "reports the file's size");
        assert_eq!(image.width(), IMAGE_MAX_SIDE, "decoded at most the cap");

        std::fs::write(d.join("bad.png"), "not a png").unwrap();
        assert!(matches!(load(&d.join("bad.png")).content, Content::Error(_)));
    }

    #[test]
    fn only_images_and_tracks_are_media() {
        assert!(is_media(Path::new("a/b.PNG")));
        assert!(is_media(Path::new("song.mp3")));
        assert!(!is_media(Path::new("notes.md")));
        assert!(!is_media(Path::new("Makefile")));
    }

    #[cfg(unix)]
    #[test]
    fn pipes_and_devices_are_described_without_being_read() {
        let d = tmpdir("media-fifo");
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
    }

    #[test]
    fn an_image_with_no_pixels_is_an_error_not_a_panic() {
        let d = tmpdir("media-empty-img");
        // A fuzzed 17x0 GIF that used to panic building mips.
        std::fs::write(d.join("empty.gif"), include_bytes!("../../media/testdata/zero-height.gif")).unwrap();
        let p = load(&d.join("empty.gif"));
        assert!(matches!(p.content, Content::Error(_)), "{:?}", p.kind);
    }

    #[test]
    fn audio_reports_its_length() {
        let d = tmpdir("media-wav");
        std::fs::write(d.join("tone.wav"), wav(8000, 8000 * 2)).unwrap();
        let p = load(&d.join("tone.wav"));
        assert_eq!(p.kind, "WAV audio");
        let Content::Audio { duration } = p.content else { panic!("audio") };
        assert_eq!(duration, Some(Duration::from_secs(2)));
        assert_eq!(clock(Duration::from_secs(65)), "1:05");
        assert_eq!(clock(Duration::from_secs(3_725)), "1:02:05");
    }

    /// Tick `v` until it stops loading (or five seconds pass).
    fn settle(h: &Harness, v: &mut MediaView) {
        let until = Instant::now() + Duration::from_secs(5);
        while v.loading() {
            assert!(Instant::now() < until, "{:?} never loaded", v.path);
            std::thread::sleep(POLL_EVERY);
            h.tick(v, Instant::now());
        }
    }

    #[test]
    fn a_panicking_load_shows_an_error_and_the_loader_carries_on() {
        fn explode(path: &Path) -> Loaded {
            if path.ends_with("bad.png") {
                panic!("decoder bug");
            }
            load(path)
        }
        let d = tmpdir("media-panic");
        std::fs::write(d.join("good.png"), media::testing::png(2, 2, [0; 4])).unwrap();
        let h = Harness::new();
        let mut v = MediaView::open(d.join("bad.png"));
        v.loader = Some(Loader::with(explode));
        v.load();
        settle(&h, &mut v);
        assert!(matches!(v.loaded().unwrap().content, Content::Error(_)));
        v.files_changed(&[(d.join("bad.png"), d.join("good.png"))]);
        settle(&h, &mut v);
        assert!(matches!(v.loaded().unwrap().content, Content::Image { .. }));
    }

    #[test]
    fn a_dead_loader_stops_the_polling() {
        let d = tmpdir("media-dead");
        let h = Harness::new();
        let mut v = MediaView::open(d.join("gone.png"));
        settle(&h, &mut v);
        // A loader whose worker has gone, with a request it will never answer.
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || drop(rx));
        while !thread.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        v.loader = Some(Loader { tx, done: Arc::default(), thread });
        v.load();
        settle(&h, &mut v);
        assert_eq!(v.deadline(&h.cfg), None, "no timer left");
        assert!(matches!(v.loaded().unwrap().content, Content::Error(_)));
        assert!(v.loader.is_none(), "the next load starts a new loader");
    }

    #[test]
    fn an_image_draws_under_its_header() {
        let d = tmpdir("media-draw");
        std::fs::write(d.join("c.png"), media::testing::png(4, 2, [0, 0, 255, 255])).unwrap();
        let mut h = Harness::new();
        let mut v = MediaView::open(d.join("c.png"));
        settle(&h, &mut v);
        let frame = h.paint(&mut v, RECT);
        assert_eq!(frame.images.len(), 1);
        let r = frame.images[0].rect;
        assert!(r.y >= DocHeader::height(crate::plugin::testing::UI), "below the header, got {r:?}");
        assert_eq!((r.w, r.h), (4.0, 2.0), "never upscaled past a pixel per logical px");
        assert!(h.texts().iter().any(|t| t == "PNG image  \u{00b7}  4 \u{00d7} 2" || t.starts_with("PNG image")));
        assert!(h.frame.view_buttons.iter().any(|b| b.2 == BTN_MORE));
    }

    /// A shown view of the track `c.wav`, loaded, with autoplay per `on`.
    fn track(on: bool) -> (Harness, MediaView, PathBuf) {
        let d = tmpdir("media-track");
        std::fs::write(d.join("c.wav"), wav(8000, 800)).unwrap();
        let mut h = Harness::new();
        h.cfg.file_browser.preview_autoplay = on;
        let mut v = MediaView::open(d.join("c.wav"));
        v.shown(true);
        settle(&h, &mut v);
        (h, v, d.join("c.wav"))
    }

    #[test]
    fn a_track_offers_play_without_autoplaying_when_off() {
        let (mut h, mut v, _) = track(false);
        let frame = h.paint(&mut v, RECT);
        let ids: Vec<u32> = frame.view_buttons.iter().map(|(_, _, id)| *id).collect();
        assert!(ids.contains(&BTN_PLAY) && ids.contains(&BTN_AUTOPLAY));
        assert_eq!(v.deadline(&h.cfg), None, "nothing scheduled: no autoplay");
        let out = h.button(&mut v, BTN_MORE);
        let Some(Request::Menu { items, .. }) = out.requests.first() else { panic!("a menu") };
        assert_eq!(items.iter().find(|i| i.id == BTN_AUTOPLAY).and_then(|i| i.checked), Some(false));
    }

    #[test]
    fn a_track_shows_the_autoplay_icon_in_its_header_when_listed() {
        let (mut h, mut v, _) = track(false);
        let header = DocHeader::height(crate::plugin::testing::UI);
        let in_header = |h: &mut Harness, v: &mut MediaView| -> Vec<u32> {
            let mut b: Vec<(Rect, u32)> = h
                .paint(v, RECT)
                .view_buttons
                .iter()
                .filter(|b| b.0.y + b.0.h <= header && b.2 != BTN_NAME)
                .map(|b| (b.0, b.2))
                .collect();
            b.sort_by(|a, b| a.0.x.total_cmp(&b.0.x));
            b.into_iter().map(|b| b.1).collect()
        };
        assert_eq!(in_header(&mut h, &mut v), [BTN_MORE], "not listed by default");
        h.cfg.editor.header_icons = vec![HeaderIcon::SoftWrap, HeaderIcon::Autoplay];
        assert_eq!(in_header(&mut h, &mut v), [BTN_AUTOPLAY, BTN_MORE]);
        let out = h.button(&mut v, BTN_AUTOPLAY);
        let set = Request::SetConfig { section: "file_browser", key: "preview_autoplay", value: toml::Value::Boolean(true) };
        assert_eq!(out.requests, [set], "the button sets the global default");
        // An image has no autoplay.
        let d = tmpdir("media-icons");
        std::fs::write(d.join("c.png"), media::testing::png(4, 2, [0, 0, 255, 255])).unwrap();
        let mut img = MediaView::open(d.join("c.png"));
        settle(&h, &mut img);
        assert_eq!(in_header(&mut h, &mut img), [BTN_MORE]);
    }

    #[test]
    fn hiding_the_view_cancels_a_pending_autoplay_and_showing_it_does_not_restore_it() {
        let (h, mut v, _) = track(true);
        assert!(v.deadline(&h.cfg).is_some(), "autoplay is pending");
        v.shown(false);
        assert_eq!(v.deadline(&h.cfg), None, "nothing scheduled while hidden");
        v.shown(true);
        assert_eq!(v.deadline(&h.cfg), None, "not re-armed on return");
    }

    #[test]
    fn a_track_that_finishes_loading_while_hidden_does_not_autoplay() {
        let d = tmpdir("media-hidden");
        std::fs::write(d.join("c.wav"), wav(8000, 800)).unwrap();
        let mut h = Harness::new();
        h.cfg.file_browser.preview_autoplay = true;
        let mut v = MediaView::open(d.join("c.wav"));
        settle(&h, &mut v);
        assert_eq!(v.deadline(&h.cfg), None, "no autoplay armed while hidden");
    }

    #[test]
    fn hiding_the_view_pauses_a_playing_track_and_showing_it_leaves_it_paused() {
        let (h, mut v, wav) = track(true);
        let player = AudioPlayer::new();
        player.play(&wav);
        v.install_player(player);
        assert!(v.audio_status().is_some_and(|s| s.playing));
        v.shown(false);
        assert!(v.audio_status().is_some_and(|s| !s.playing), "paused on hide");
        v.shown(true);
        assert!(v.audio_status().is_some_and(|s| !s.playing), "not resumed on show");
        assert_eq!(v.deadline(&h.cfg), None);
    }
}
