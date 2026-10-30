//! Toasts: coloured notices across the top of the pane they concern, or of
//! the workspace area for the whole app. One window-wide FIFO queue, of which
//! only the head shows. Errors and warnings stay until dismissed; info and
//! success hide after [`AUTO_HIDE`] on screen.
//!
//! The queue is [`AppState`](crate::app_state::AppState)'s; the window places,
//! paints, and hit-tests the head with [`place`] and [`paint`].

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use ghostrealm_core::{PaneId, Rect};

use crate::app_state::DocRef;
use crate::plugin::icons::Icon;
use crate::plugin::{theme, Font, PaintCx, Shaped};
use crate::window::wrap_words;

/// How long an info or success toast shows before it hides.
pub const AUTO_HIDE: Duration = Duration::from_secs(4);
/// Toasts kept without a window: nobody can dismiss them, so the oldest go.
const HEADLESS_CAP: usize = 32;
/// Most message lines a toast shows; the last is cut with an ellipsis.
const MAX_LINES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warning,
    Error,
}

impl Level {
    /// Whether a toast of this level stays until dismissed.
    pub fn sticky(self) -> bool {
        matches!(self, Level::Warning | Level::Error)
    }

    /// The toast's fill.
    pub fn color(self) -> [u8; 3] {
        match self {
            Level::Info => theme::TOAST_INFO,
            Level::Success => theme::TOAST_SUCCESS,
            Level::Warning => theme::TOAST_WARNING,
            Level::Error => theme::TOAST_ERROR,
        }
    }
}

/// What a toast is about: one pane, or the whole app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    App,
    Pane(PaneId),
}

/// What one of a toast's choices does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToastAction {
    /// Open the config file, caret at this line (1-based) when given.
    OpenSettings { line: Option<usize> },
    /// Ask where to save this document (the Save As picker).
    SaveAs(DocRef),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub level: Level,
    pub message: String,
    pub scope: Scope,
    /// A toast with a key replaces the queued one with the same key.
    pub key: Option<String>,
    /// Buttons beside the message: label and action.
    pub choices: Vec<(String, ToastAction)>,
    /// Stays until dismissed (else hides after [`AUTO_HIDE`]).
    pub sticky: bool,
}

impl Toast {
    /// An app-wide toast, sticky as its level says.
    pub fn new(level: Level, message: impl Into<String>) -> Self {
        Toast {
            level,
            message: message.into(),
            scope: Scope::App,
            key: None,
            choices: Vec::new(),
            sticky: level.sticky(),
        }
    }

    pub fn on(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }

    pub fn keyed(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn with_choice(mut self, label: impl Into<String>, action: ToastAction) -> Self {
        self.choices.push((label.into(), action));
        self
    }

    /// Whether `other` takes this one's place in the queue: the same key, or
    /// with no key, the same scope and message.
    fn replaced_by(&self, other: &Toast) -> bool {
        match &other.key {
            Some(k) => self.key.as_ref() == Some(k),
            None => self.scope == other.scope && self.message == other.message,
        }
    }
}

/// A toast's text for a failed write of `path`.
pub fn save_failed(path: &Path, e: &anyhow::Error) -> String {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    format!("Couldn't save {name}: {e:#}")
}

/// The key of the toast for a failed write of `path`: a save and an
/// autosave of one file share it, and a successful write of it dismisses it.
pub fn save_key(path: &Path) -> String {
    format!("save:{}", path.display())
}

/// A queued toast.
#[derive(Debug)]
pub struct Entry {
    pub id: u64,
    pub toast: Toast,
    /// How long it has been visible (only the head's clock runs).
    shown_for: Duration,
}

/// The window's toast queue.
#[derive(Debug, Default)]
pub struct Toasts {
    queue: VecDeque<Entry>,
    next_id: u64,
    /// A window shows the queue, so nothing is dropped.
    windowed: bool,
    /// The head's id and since when it has been visible, while it is.
    visible_since: Option<(u64, Instant)>,
}

impl Toasts {
    /// A window shows the queue: keep every toast until it is dismissed.
    pub fn set_windowed(&mut self, on: bool) {
        self.windowed = on;
    }

    /// Queue `toast`, or put it in the place of the one it replaces
    /// ([`Toast::key`]), with its clock reset. Returns its id.
    pub fn push(&mut self, toast: Toast) -> u64 {
        if let Some(e) = self.queue.iter_mut().find(|e| e.toast.replaced_by(&toast)) {
            e.toast = toast;
            e.shown_for = Duration::ZERO;
            let id = e.id;
            self.restart_clock(id);
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.queue.push_back(Entry { id, toast, shown_for: Duration::ZERO });
        if !self.windowed {
            while self.queue.len() > HEADLESS_CAP {
                self.queue.pop_front();
            }
        }
        id
    }

    /// The toast that shows.
    pub fn head(&self) -> Option<&Entry> {
        self.queue.front()
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.queue.iter().find(|e| e.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.queue.iter()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Remove toast `id`. Returns whether it was queued.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.queue.len();
        self.queue.retain(|e| e.id != id);
        self.queue.len() != before
    }

    /// Remove the toast keyed `key` ([`Toast::key`]). Returns whether one
    /// was queued.
    pub fn dismiss_key(&mut self, key: &str) -> bool {
        let before = self.queue.len();
        self.queue.retain(|e| e.toast.key.as_deref() != Some(key));
        self.queue.len() != before
    }

    /// Put `toast` in toast `id`'s place, its clock reset. Returns whether
    /// `id` was queued.
    pub fn replace(&mut self, id: u64, toast: Toast) -> bool {
        let Some(e) = self.queue.iter_mut().find(|e| e.id == id) else { return false };
        e.toast = toast;
        e.shown_for = Duration::ZERO;
        self.restart_clock(id);
        true
    }

    /// Toast `id` has new content: none of the time before it counts, so
    /// its visible stretch starts at the next tick.
    fn restart_clock(&mut self, id: u64) {
        if self.visible_since.is_some_and(|(v, _)| v == id) {
            self.visible_since = None;
        }
    }

    /// Advance the head's clock to `now`, counting the time since the last
    /// tick if the head was visible then, and note whether it is `visible`
    /// from now on. A non-sticky head that has shown for [`AUTO_HIDE`] goes.
    /// Returns whether it did.
    pub fn tick(&mut self, now: Instant, visible: bool) -> bool {
        let Some(head) = self.queue.front_mut() else {
            self.visible_since = None;
            return false;
        };
        if let Some((id, since)) = self.visible_since {
            if id == head.id {
                head.shown_for += now.saturating_duration_since(since);
            }
        }
        if !head.toast.sticky && head.shown_for >= AUTO_HIDE {
            self.queue.pop_front();
            self.visible_since = None;
            return true;
        }
        self.visible_since = visible.then_some((head.id, now));
        false
    }

    /// When the visible head is due to hide, if it hides by itself.
    pub fn next_deadline(&self) -> Option<Instant> {
        let head = self.queue.front()?;
        let (id, since) = self.visible_since?;
        (id == head.id && !head.toast.sticky).then(|| since + AUTO_HIDE.saturating_sub(head.shown_for))
    }
}

/// Where toasts can go this frame.
pub struct Area<'a> {
    /// The workspace area (the window minus the sidebar and title strip).
    pub workspace: Rect,
    /// Each pane on screen: its id, its rect, and its content rect (below
    /// its tab strip).
    pub panes: &'a [(PaneId, Rect, Rect)],
    /// The sidebar toggle, which can float over the workspace's top corner.
    pub toggle: Option<Rect>,
    /// A pane narrower than this shows its toasts across the workspace.
    pub min_pane_w: f32,
    /// Space between a toast and the edges of its area.
    pub margin: f32,
}

/// The rect a toast of `scope` may fill: across the top of its pane's
/// content, or of the workspace below the tab strips of its top row of
/// panes, inset by the margin and clear of the floating sidebar toggle. A
/// pane that isn't on screen, or is too narrow, gives the workspace's.
/// [`paint`] takes the height it needs from the top.
pub fn place(scope: Scope, a: &Area) -> Rect {
    let pane = match scope {
        Scope::Pane(id) => a.panes.iter().find(|(p, _, c)| *p == id && c.w >= a.min_pane_w).map(|(_, _, c)| *c),
        Scope::App => None,
    };
    let region = pane.unwrap_or_else(|| {
        let ws = a.workspace;
        let top = a
            .panes
            .iter()
            .filter(|(_, r, _)| (r.y - ws.y).abs() < 0.5)
            .map(|(_, _, c)| c.y)
            .fold(ws.y, f32::max);
        Rect { x: ws.x, y: top, w: ws.w, h: (ws.y + ws.h - top).max(0.0) }
    });
    let m = a.margin;
    let mut r = Rect {
        x: region.x + m,
        y: region.y + m,
        w: (region.w - 2.0 * m).max(1.0),
        h: (region.h - 2.0 * m).max(1.0),
    };
    if let Some(t) = a.toggle {
        let overlaps = t.x < r.x + r.w && t.x + t.w > r.x && t.y < r.y + r.h && t.y + t.h > r.y;
        if overlaps {
            if t.x + t.w * 0.5 < r.x + r.w * 0.5 {
                let left = t.x + t.w + m;
                r.w = (r.x + r.w - left).max(1.0);
                r.x = left;
            } else {
                r.w = (t.x - m - r.x).max(1.0);
            }
        }
    }
    r
}

/// A clickable part of a toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastHit {
    /// The `×`.
    Dismiss,
    /// The choice at this index.
    Choice(usize),
}

/// A painted toast: its rect and its buttons.
#[derive(Debug)]
pub struct ToastLayout {
    pub rect: Rect,
    pub buttons: Vec<(Rect, ToastHit)>,
}

/// Paint `toast` across the top of `area` ([`place`]): its message wrapped
/// to at most three lines, then right-aligned its choices, "+`more`" when
/// more are queued, and `×`. A cut message is the toast's tooltip in full.
/// Unless `choices_live`, the choices draw dim and take neither hover nor
/// clicks; `×` always does.
pub fn paint(cx: &mut PaintCx, toast: &Toast, area: Rect, more: usize, choices_live: bool) -> ToastLayout {
    let ui = cx.ui;
    let (s, ch) = (ui.scale, ui.cell_h);
    let (pad, vpad, gap) = (8.0 * s, 6.0 * s, 6.0 * s);
    let btn_h = ch + 4.0 * s;
    let close_w = btn_h;
    let choice_pad = 8.0 * s;

    // The controls are measured first: the message wraps in what's left.
    let choices: Vec<Shaped> = toast.choices.iter().map(|(label, _)| cx.shape(label, Font::SANS, area.w)).collect();
    let more_label = (more > 0).then(|| cx.shape(&format!("+{more}"), Font::SANS, area.w));
    let controls_w = choices.iter().map(|c| c.width + 2.0 * choice_pad + gap).sum::<f32>()
        + more_label.map_or(0.0, |m| m.width + gap)
        + close_w;
    let text_w = (area.w - 2.0 * pad - controls_w - gap).max(ch);

    let mut lines = Vec::new();
    for para in toast.message.split('\n') {
        // A blank line would spend one of the few lines on nothing.
        lines.extend(
            wrap_words(para, text_w, |t| cx.shape(t, Font::SANS, f32::MAX).width).into_iter().filter(|l| !l.is_empty()),
        );
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    let cut = lines.len() > MAX_LINES;
    if cut {
        let rest = lines[MAX_LINES - 1..].join(" ");
        lines.truncate(MAX_LINES - 1);
        let mut last = cx.cut_to_fit(&rest, Font::SANS, text_w);
        if !last.ends_with('\u{2026}') {
            last.push('\u{2026}');
        }
        lines.push(last);
    }

    let h = lines.len() as f32 * ch + 2.0 * vpad;
    let rect = Rect { x: area.x.round(), y: area.y.round(), w: area.w.round(), h: h.round() };
    let fill = toast.level.color();
    let edge = fill.map(|c| c + (255 - c) / 4);
    cx.round_rect(rect, theme::pill_radius(ui), fill, 1.0, Some((theme::hairline(ui), edge, 1.0)));

    let text_clip = Rect { x: rect.x, y: rect.y, w: pad + text_w, h: rect.h };
    for (i, line) in lines.iter().enumerate() {
        let shaped = cx.shape(line, Font::SANS, text_w + pad);
        cx.place(shaped, rect.x + pad, rect.y + vpad + i as f32 * ch, text_clip, theme::TEXT);
    }
    if cut {
        cx.tooltip(text_clip, toast.message.clone());
    }

    // Right to left: ×, +N, then the choices, last first.
    let mut buttons = Vec::new();
    let by = rect.y + (rect.h - btn_h) * 0.5;
    let label_y = rect.y + (rect.h - ch) * 0.5;
    let mut right = rect.x + rect.w - pad * 0.5;
    let close = Rect { x: right - close_w, y: by, w: close_w, h: btn_h };
    let hovered = cx.hovered(close);
    if hovered {
        cx.highlight(close);
    }
    Icon::Close.paint(cx, close, if hovered { theme::GLYPH_HOVER } else { theme::TEXT });
    cx.tooltip(close, "Dismiss");
    buttons.push((close, ToastHit::Dismiss));
    right = close.x - gap;
    if let Some(m) = more_label {
        cx.place(m, right - m.width, label_y, rect, theme::LABEL_HOVER);
        right -= m.width + gap;
    }
    for (i, label) in choices.iter().enumerate().rev() {
        let w = label.width + 2.0 * choice_pad;
        let hit = Rect { x: right - w, y: by, w, h: btn_h };
        cx.fill_alpha(hit, theme::HOVER_BG, if choices_live { 0.12 } else { 0.05 });
        if choices_live && cx.hovered(hit) {
            cx.highlight(hit);
        }
        cx.place(*label, hit.x + choice_pad, label_y, hit, if choices_live { theme::TEXT } else { theme::DIM });
        if choices_live {
            buttons.push((hit, ToastHit::Choice(i)));
        }
        right = hit.x - gap;
    }
    ToastLayout { rect, buttons }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::{Harness, UI};
    use crate::plugin::{Frame, Layer};
    use ghostrealm_core::SurfaceId;

    fn t0() -> Instant {
        Instant::now()
    }

    fn secs(s: f32) -> Duration {
        Duration::from_secs_f32(s)
    }

    fn messages(t: &Toasts) -> Vec<&str> {
        t.iter().map(|e| e.toast.message.as_str()).collect()
    }

    #[test]
    fn the_queue_shows_its_oldest_first_and_dismiss_advances() {
        let mut t = Toasts::default();
        let a = t.push(Toast::new(Level::Error, "a"));
        t.push(Toast::new(Level::Error, "b"));
        assert_eq!(t.head().map(|e| e.id), Some(a));
        assert!(t.dismiss(a));
        assert_eq!(messages(&t), ["b"]);
        assert!(!t.dismiss(a));
    }

    #[test]
    fn a_key_or_the_same_scope_and_message_replaces_in_place() {
        let mut t = Toasts::default();
        let first = t.push(Toast::new(Level::Error, "bad config").keyed("config"));
        t.push(Toast::new(Level::Error, "other"));
        let again = t.push(Toast::new(Level::Success, "Settings applied").keyed("config"));
        assert_eq!(again, first);
        assert_eq!(messages(&t), ["Settings applied", "other"]);
        // No key: the same scope and message replace; another scope doesn't.
        t.push(Toast::new(Level::Error, "other"));
        assert_eq!(t.len(), 2);
        t.push(Toast::new(Level::Error, "other").on(Scope::Pane(PaneId(3))));
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn dismissing_a_key_removes_only_its_toast() {
        let mut t = Toasts::default();
        t.push(Toast::new(Level::Error, "a"));
        t.push(Toast::new(Level::Warning, "font").keyed("preview_font"));
        assert!(t.dismiss_key("preview_font"));
        assert_eq!(messages(&t), ["a"]);
        assert!(!t.dismiss_key("preview_font"));
    }

    #[test]
    fn a_replacement_restarts_the_clock() {
        let mut t = Toasts::default();
        let now = t0();
        t.push(Toast::new(Level::Info, "x").keyed("k"));
        t.tick(now, true);
        t.tick(now + secs(3.0), true);
        t.push(Toast::new(Level::Info, "y").keyed("k"));
        // Its clock starts at the first tick after the replacement.
        assert!(!t.tick(now + secs(4.5), true));
        assert!(!t.tick(now + secs(8.4), true));
        assert!(t.tick(now + secs(8.6), true));
    }

    #[test]
    fn a_replacement_of_the_visible_head_is_not_charged_for_time_before_it() {
        let mut t = Toasts::default();
        let now = t0();
        let id = t.push(Toast::new(Level::Error, "bad config").keyed("config"));
        t.tick(now, true);
        // Long after the last tick, the head is replaced.
        t.push(Toast::new(Level::Success, "Settings applied").keyed("config"));
        assert!(!t.tick(now + secs(60.0), true));
        assert!(!t.tick(now + secs(63.9), true));
        assert!(t.tick(now + secs(64.1), true));
        // Likewise by id.
        let id2 = t.push(Toast::new(Level::Error, "x"));
        assert_ne!(id, id2);
        t.tick(now + secs(70.0), true);
        t.replace(id2, Toast::new(Level::Info, "y"));
        assert!(!t.tick(now + secs(130.0), true));
        assert!(t.tick(now + secs(134.1), true));
    }

    #[test]
    fn info_and_success_hide_after_four_visible_seconds_and_sticky_ones_stay() {
        let now = t0();
        for level in [Level::Info, Level::Success] {
            let mut t = Toasts::default();
            t.push(Toast::new(level, "x"));
            assert!(!t.tick(now, true));
            assert!(!t.tick(now + secs(3.9), true));
            assert!(t.tick(now + secs(4.1), true));
            assert!(t.is_empty());
        }
        for level in [Level::Warning, Level::Error] {
            let mut t = Toasts::default();
            t.push(Toast::new(level, "x"));
            t.tick(now, true);
            assert!(!t.tick(now + secs(60.0), true));
            assert_eq!(t.len(), 1);
            assert_eq!(t.next_deadline(), None);
        }
    }

    #[test]
    fn time_hidden_does_not_count() {
        // Hidden: drawn under a modal, the window unfocused, or the pointer
        // over the toast.
        let now = t0();
        let mut t = Toasts::default();
        t.push(Toast::new(Level::Info, "x"));
        t.tick(now, true);
        t.tick(now + secs(3.0), false);
        assert!(!t.tick(now + secs(30.0), true));
        assert!(!t.tick(now + secs(30.9), true));
        assert!(t.tick(now + secs(31.1), true));
    }

    #[test]
    fn the_next_head_starts_its_own_clock() {
        let now = t0();
        let mut t = Toasts::default();
        let a = t.push(Toast::new(Level::Error, "a"));
        t.push(Toast::new(Level::Info, "b"));
        t.tick(now, true);
        t.dismiss(a);
        // The time `a` showed isn't `b`'s.
        assert!(!t.tick(now + secs(10.0), true));
        assert!(t.tick(now + secs(14.1), true));
    }

    #[test]
    fn the_deadline_is_when_the_visible_head_hides() {
        let now = t0();
        let mut t = Toasts::default();
        assert_eq!(t.next_deadline(), None);
        t.push(Toast::new(Level::Info, "x"));
        assert_eq!(t.next_deadline(), None);
        t.tick(now, true);
        assert_eq!(t.next_deadline(), Some(now + AUTO_HIDE));
        t.tick(now + secs(1.0), false);
        assert_eq!(t.next_deadline(), None);
        t.tick(now + secs(5.0), true);
        assert_eq!(t.next_deadline(), Some(now + secs(5.0) + secs(3.0)));
    }

    #[test]
    fn a_window_keeps_every_toast_and_headless_keeps_the_latest_32() {
        let mut windowed = Toasts::default();
        windowed.set_windowed(true);
        let mut headless = Toasts::default();
        for i in 0..40 {
            windowed.push(Toast::new(Level::Error, format!("e{i}")));
            headless.push(Toast::new(Level::Error, format!("e{i}")));
        }
        assert_eq!(windowed.len(), 40);
        assert_eq!(headless.len(), 32);
        assert_eq!(headless.head().map(|e| e.toast.message.as_str()), Some("e8"));
    }

    const WS: Rect = Rect { x: 200.0, y: 28.0, w: 800.0, h: 600.0 };
    const STRIP: f32 = 22.0;

    fn pane(id: u64, x: f32, y: f32, w: f32, h: f32, strip: f32) -> (PaneId, Rect, Rect) {
        let r = Rect { x, y, w, h };
        (PaneId(id), r, Rect { x, y: y + strip, w, h: h - strip })
    }

    fn area(panes: &[(PaneId, Rect, Rect)], toggle: Option<Rect>) -> Area<'_> {
        Area { workspace: WS, panes, toggle, min_pane_w: 192.0, margin: 6.0 }
    }

    #[test]
    fn an_app_toast_sits_below_the_top_rows_tallest_strip() {
        // Left: no strip; right top: a strip; right bottom: a taller one,
        // not in the top row.
        let panes = [
            pane(1, 200.0, 28.0, 400.0, 600.0, 0.0),
            pane(2, 606.0, 28.0, 394.0, 297.0, STRIP),
            pane(3, 606.0, 331.0, 394.0, 297.0, 40.0),
        ];
        let r = place(Scope::App, &area(&panes, None));
        assert_eq!((r.x, r.y, r.w), (206.0, 28.0 + STRIP + 6.0, 788.0));
    }

    #[test]
    fn an_app_toast_clears_the_floating_sidebar_toggle() {
        let panes = [pane(1, 200.0, 28.0, 800.0, 600.0, 0.0)];
        let left = Rect { x: 200.0, y: 28.0, w: 24.0, h: 24.0 };
        let r = place(Scope::App, &area(&panes, Some(left)));
        assert_eq!((r.x, r.y), (230.0, 34.0));
        assert_eq!(r.x + r.w, 994.0);
        let right = Rect { x: 976.0, y: 28.0, w: 24.0, h: 24.0 };
        let r = place(Scope::App, &area(&panes, Some(right)));
        assert_eq!((r.x, r.x + r.w), (206.0, 970.0));
        // A toggle in the title strip, above the workspace, doesn't move it.
        let above = Rect { x: 200.0, y: 0.0, w: 28.0, h: 28.0 };
        assert_eq!(place(Scope::App, &area(&panes, Some(above))).x, 206.0);
    }

    #[test]
    fn a_pane_toast_sits_below_its_panes_strip() {
        let panes = [pane(1, 200.0, 28.0, 400.0, 600.0, 0.0), pane(2, 606.0, 28.0, 394.0, 600.0, STRIP)];
        let r = place(Scope::Pane(PaneId(2)), &area(&panes, None));
        assert_eq!((r.x, r.y, r.w), (612.0, 28.0 + STRIP + 6.0, 382.0));
    }

    #[test]
    fn a_pane_toast_clears_the_floating_sidebar_toggle() {
        let panes = [pane(1, 200.0, 28.0, 400.0, 600.0, 0.0), pane(2, 606.0, 28.0, 394.0, 600.0, STRIP)];
        let toggle = Rect { x: 200.0, y: 28.0, w: 24.0, h: 24.0 };
        let r = place(Scope::Pane(PaneId(1)), &area(&panes, Some(toggle)));
        assert_eq!((r.x, r.y, r.x + r.w), (230.0, 34.0, 594.0));
        // The pane beside it is clear of the toggle.
        let r = place(Scope::Pane(PaneId(2)), &area(&panes, Some(toggle)));
        assert_eq!((r.x, r.x + r.w), (612.0, 994.0));
    }

    #[test]
    fn a_narrow_or_missing_pane_falls_back_to_the_workspace() {
        let panes = [pane(1, 200.0, 28.0, 100.0, 600.0, STRIP), pane(2, 306.0, 28.0, 694.0, 600.0, STRIP)];
        let app = place(Scope::App, &area(&panes, None));
        assert_eq!(place(Scope::Pane(PaneId(1)), &area(&panes, None)), app);
        assert_eq!(place(Scope::Pane(PaneId(9)), &area(&panes, None)), app);
    }

    /// Paint `toast` at the top of an 800-wide screen with the pointer at
    /// `cursor`, in a frame of its own (as the window does).
    fn paint_at(h: &mut Harness, toast: &Toast, w: f32, more: usize, cursor: (f32, f32)) -> ToastLayout {
        h.frame = Frame::new((800.0, 600.0));
        h.text.begin_frame(0, glyphon::Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let mut cx = PaintCx::new(&h.cfg, &h.chrome, UI, cursor, &mut h.text, &mut h.frame, Layer::Overlay, SurfaceId(1));
        let layout = paint(&mut cx, toast, Rect { x: 10.0, y: 10.0, w, h: 500.0 }, more, true);
        h.text.finish_paint();
        layout
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x && inner.y >= outer.y && inner.x + inner.w <= outer.x + outer.w && inner.y + inner.h <= outer.y + outer.h
    }

    fn centre(r: Rect) -> (f32, f32) {
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    #[test]
    fn a_long_message_wraps_to_three_lines_with_an_ellipsis_and_its_full_text_as_tooltip() {
        let mut h = Harness::new();
        let message = "word ".repeat(80).trim_end().to_string();
        let toast = Toast::new(Level::Error, message.clone());
        let layout = paint_at(&mut h, &toast, 400.0, 0, (20.0, 20.0));
        let lines: Vec<String> = h.texts().into_iter().filter(|t| t.starts_with("word")).collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[2].ends_with('\u{2026}'));
        assert_eq!(layout.rect.h, 3.0 * UI.cell_h + 12.0);
        assert_eq!(h.frame.tooltip.as_ref().map(|t| t.1.as_str()), Some(message.as_str()));
        // A short one is one line, and has no tooltip of its own.
        let layout = paint_at(&mut h, &Toast::new(Level::Error, "short"), 400.0, 0, (20.0, 20.0));
        assert_eq!(layout.rect.h, UI.cell_h + 12.0);
        assert!(h.frame.tooltip.is_none());
    }

    #[test]
    fn blank_lines_take_no_line_of_the_toast() {
        let mut h = Harness::new();
        let toast = Toast::new(Level::Error, "Couldn't open a.txt: denied\n\nUnsaved changes");
        let layout = paint_at(&mut h, &toast, 400.0, 0, (20.0, 20.0));
        assert_eq!(layout.rect.h, 2.0 * UI.cell_h + 12.0);
        // A message that is only blank still shows as one line.
        let layout = paint_at(&mut h, &Toast::new(Level::Error, "\n"), 400.0, 0, (20.0, 20.0));
        assert_eq!(layout.rect.h, UI.cell_h + 12.0);
    }

    #[test]
    fn the_buttons_sit_inside_the_toast_and_only_the_hovered_one_lights_up() {
        let mut h = Harness::new();
        let toast = Toast::new(Level::Warning, "config.toml: 2 problems")
            .with_choice("Open Settings", ToastAction::OpenSettings { line: Some(3) });
        let layout = paint_at(&mut h, &toast, 600.0, 0, (-1.0, -1.0));
        let hits: Vec<ToastHit> = layout.buttons.iter().map(|(_, hit)| *hit).collect();
        assert_eq!(hits, [ToastHit::Dismiss, ToastHit::Choice(0)]);
        assert!(layout.buttons.iter().all(|(r, _)| inside(*r, layout.rect)));
        let lit = |h: &Harness| {
            let a = theme::HOVER_ALPHA;
            h.frame.overlay_quads().filter(|q| (q.color[3] - a).abs() < 1e-4).count()
        };
        assert_eq!(lit(&h), 0);
        let close = layout.buttons[0].0;
        paint_at(&mut h, &toast, 600.0, 0, centre(close));
        assert_eq!(lit(&h), 1);
        assert_eq!(h.frame.tooltip.as_ref().map(|t| t.1.as_str()), Some("Dismiss"));
    }

    /// Inert choices (a picker or confirmation is open) still show but take
    /// neither hover nor clicks; `×` stays live.
    #[test]
    fn inert_choices_show_but_only_dismiss_is_a_button() {
        let mut h = Harness::new();
        let toast = Toast::new(Level::Warning, "config.toml: 2 problems")
            .with_choice("Open Settings", ToastAction::OpenSettings { line: None });
        let live = paint_at(&mut h, &toast, 600.0, 0, (-1.0, -1.0));
        let choice = live.buttons[1].0;
        h.frame = Frame::new((800.0, 600.0));
        h.text.begin_frame(0, glyphon::Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let mut cx =
            PaintCx::new(&h.cfg, &h.chrome, UI, centre(choice), &mut h.text, &mut h.frame, Layer::Overlay, SurfaceId(1));
        let layout = paint(&mut cx, &toast, Rect { x: 10.0, y: 10.0, w: 600.0, h: 500.0 }, 0, false);
        h.text.finish_paint();
        let hits: Vec<ToastHit> = layout.buttons.iter().map(|(_, hit)| *hit).collect();
        assert_eq!(hits, [ToastHit::Dismiss]);
        let a = theme::HOVER_ALPHA;
        assert_eq!(h.frame.overlay_quads().filter(|q| (q.color[3] - a).abs() < 1e-4).count(), 0, "no hover");
        assert!(h.frame.overlay_text().any(|t| t.color == theme::DIM), "the label shows, dim");
    }

    /// A toast painted in a layer begun over a modal's draws over all of the
    /// modal, its text too: its panel first, then its own text.
    #[test]
    fn a_toast_draws_over_a_modal_in_a_layer_of_its_own() {
        use crate::plugin::Pass;
        let mut h = Harness::new();
        h.frame = Frame::new((800.0, 600.0));
        h.text.begin_frame(0, glyphon::Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let modal = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        h.frame.begin_layer();
        {
            let mut cx = PaintCx::new(&h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Overlay, SurfaceId(1));
            cx.fill(modal, [0; 3]);
            let s = cx.shape("Replace a.txt?", Font::SANS, 400.0);
            cx.place(s, 20.0, 20.0, modal, [255; 3]);
        }
        h.frame.begin_layer();
        {
            let mut cx = PaintCx::new(&h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Overlay, SurfaceId(1));
            paint(&mut cx, &Toast::new(Level::Error, "Save failed"), Rect { x: 10.0, y: 10.0, w: 600.0, h: 500.0 }, 0, true);
        }
        h.text.finish_paint();
        let (_, passes) = h.frame.passes(|layer| layer == 0);
        let at = |p: &Pass| passes.iter().position(|q| q == p).unwrap_or_else(|| panic!("{p:?} in {passes:?}"));
        let toast_quads = passes.iter().rposition(|p| matches!(p, Pass::Quads(r) if r.start > 0)).expect("toast quads");
        assert!(at(&Pass::Text(2)) < toast_quads, "the modal's text draws before the toast's panel: {passes:?}");
        assert!(toast_quads < at(&Pass::Text(3)), "the toast's text draws over its panel: {passes:?}");
        assert_eq!(passes.last(), Some(&Pass::Text(3)));
        assert!(h.texts().iter().any(|t| t == "Save failed"));
    }

    #[test]
    fn more_queued_shows_a_count() {
        let mut h = Harness::new();
        let toast = Toast::new(Level::Info, "hello");
        paint_at(&mut h, &toast, 600.0, 2, (-1.0, -1.0));
        assert!(h.texts().iter().any(|t| t == "+2"));
        paint_at(&mut h, &toast, 600.0, 0, (-1.0, -1.0));
        assert!(!h.texts().iter().any(|t| t.starts_with('+')));
    }
}
