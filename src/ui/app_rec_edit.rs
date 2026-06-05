//! Recording editor subpage: shows a saved recording as a waveform, lets the
//! user click to place the playhead, drag to mark a region, zoom with the +/-
//! buttons or the scroll wheel, pan the zoomed view with the scrollbar, delete
//! the marked region (scissors), preview from the playhead, and finally save —
//! which destructively re-encodes the recording without the cut ranges (see
//! [`crate::core::waveform`]).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::i18n::gettext;
use crate::ui::app::{App, Msg};

/// Number of waveform columns decoded for the display.
const BUCKETS: usize = 1200;
/// Maximum zoom factor (1.0 = the whole recording fills the width).
const MAX_ZOOM: f64 = 50.0;
/// Pointer movement (px) below which a drag counts as a click.
const CLICK_SLOP: f64 = 4.0;

/// Mutable editor state shared between the drawing area and the controls.
struct EditState {
    /// Per-column peak amplitudes (0.0–1.0); empty until decoded.
    peaks: Vec<f32>,
    /// Total duration in seconds; 0.0 until decoded.
    duration: f64,
    /// Pending selection (start, end) in seconds, before it is cut.
    sel: Option<(f64, f64)>,
    /// Committed cut ranges (seconds) to be removed on save.
    cuts: Vec<(f64, f64)>,
    /// Playhead position (seconds): click target and play-from position.
    playhead: f64,
    /// Horizontal zoom factor (>= 1.0).
    zoom: f64,
    /// Left edge of the visible window, in seconds.
    view_start: f64,
}

impl EditState {
    /// Duration (seconds) of the currently visible window.
    fn visible(&self) -> f64 {
        self.duration / self.zoom.max(1.0)
    }
    /// Maps a widget x (0..w) to a time in seconds within the visible window.
    fn x_to_time(&self, x: f64, w: f64) -> f64 {
        if self.duration <= 0.0 || w <= 0.0 {
            return 0.0;
        }
        (self.view_start + (x / w).clamp(0.0, 1.0) * self.visible()).clamp(0.0, self.duration)
    }
    /// Maps a time (seconds) to a widget x within the visible window.
    fn time_to_x(&self, t: f64, w: f64) -> f64 {
        let vis = self.visible();
        if vis <= 0.0 {
            return 0.0;
        }
        (t - self.view_start) / vis * w
    }
    /// Zooms by `factor` around the focal widget x, keeping that point fixed.
    fn zoom_at(&mut self, factor: f64, focal_x: f64, w: f64) {
        if self.duration <= 0.0 || w <= 0.0 {
            return;
        }
        let frac = (focal_x / w).clamp(0.0, 1.0);
        let focal_t = self.view_start + frac * self.visible();
        self.zoom = (self.zoom * factor).clamp(1.0, MAX_ZOOM);
        let vis = self.visible();
        self.view_start = (focal_t - frac * vis).clamp(0.0, (self.duration - vis).max(0.0));
    }
}

/// Mirrors the view window onto the pan scrollbar: thumb size = visible window,
/// thumb position = `view_start`. Hidden when the whole recording is visible.
/// Must run with no active borrow on the state (setting the value re-enters the
/// scrollbar's value handler).
fn sync_scrollbar(
    adj: &gtk::Adjustment,
    scrollbar: &gtk::Scrollbar,
    area: &gtk::DrawingArea,
    state: &Rc<RefCell<EditState>>,
) {
    let (vis, dur, vstart, zoom) = {
        let st = state.borrow();
        (st.visible(), st.duration, st.view_start, st.zoom)
    };
    adj.set_lower(0.0);
    adj.set_upper(dur.max(vis));
    adj.set_page_size(vis);
    adj.set_step_increment(vis * 0.1);
    adj.set_page_increment(vis * 0.9);
    adj.set_value(vstart);
    scrollbar.set_visible(zoom > 1.001 && dur > 0.0);
    area.queue_draw();
}

impl App {
    pub(crate) fn open_recording_edit(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(rec) = self
            .streaming
            .recording_items
            .iter()
            .find(|r| r.id == id)
            .cloned()
        else {
            return;
        };
        let path = rec.path.clone();
        if !std::path::Path::new(&path).exists() {
            self.toast(&gettext("File not found"));
            return;
        }

        let state = Rc::new(RefCell::new(EditState {
            peaks: Vec::new(),
            duration: 0.0,
            sel: None,
            cuts: Vec::new(),
            playhead: 0.0,
            zoom: 1.0,
            view_start: 0.0,
        }));

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Top controls: scissors (left), play (right).
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let scissors = gtk::Button::from_icon_name("edit-cut-symbolic");
        scissors.set_tooltip_text(Some(&gettext("Delete the marked part")));
        scissors.add_css_class("flat");
        let top_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        top_spacer.set_hexpand(true);
        let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
        play.set_tooltip_text(Some(&gettext("Play from the playhead")));
        play.add_css_class("flat");
        top.append(&scissors);
        top.append(&top_spacer);
        top.append(&play);
        content.append(&top);

        // Waveform.
        let area = gtk::DrawingArea::new();
        area.set_content_height(200);
        area.set_hexpand(true);
        area.add_css_class("card");
        {
            let state = state.clone();
            area.set_draw_func(move |_, cr, w, h| draw_waveform(cr, w, h, &state.borrow()));
        }
        content.append(&area);

        // Pan scrollbar: moves the zoomed window; hidden when fully zoomed out.
        let adj = gtk::Adjustment::new(0.0, 0.0, 1.0, 0.1, 0.9, 1.0);
        let scrollbar = gtk::Scrollbar::new(gtk::Orientation::Horizontal, Some(&adj));
        scrollbar.set_visible(false);
        {
            let state = state.clone();
            let area2 = area.clone();
            adj.connect_value_changed(move |a| {
                state.borrow_mut().view_start = a.value();
                area2.queue_draw();
            });
        }
        content.append(&scrollbar);

        // Timeline (synced with the waveform): a global position picker.
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        {
            let state = state.clone();
            let area2 = area.clone();
            scale.connect_value_changed(move |s| {
                state.borrow_mut().playhead = s.value();
                area2.queue_draw();
            });
        }
        content.append(&scale);

        // Bottom: zoom −/+ (left), reset + save (right).
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let zoom_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        zoom_box.add_css_class("linked");
        let zoom_out = gtk::Button::from_icon_name("zoom-out-symbolic");
        zoom_out.set_tooltip_text(Some(&gettext("Zoom out")));
        let zoom_in = gtk::Button::from_icon_name("zoom-in-symbolic");
        zoom_in.set_tooltip_text(Some(&gettext("Zoom in")));
        zoom_box.append(&zoom_out);
        zoom_box.append(&zoom_in);
        let bottom_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bottom_spacer.set_hexpand(true);
        let reset = gtk::Button::with_label(&gettext("Reset"));
        let save = gtk::Button::with_label(&gettext("Save"));
        save.add_css_class("suggested-action");
        bottom.append(&zoom_box);
        bottom.append(&bottom_spacer);
        bottom.append(&reset);
        bottom.append(&save);
        content.append(&bottom);

        // Track the pointer x for scroll-zoom focal point.
        let last_x = Rc::new(Cell::new(0.0f64));
        let motion = gtk::EventControllerMotion::new();
        {
            let last_x = last_x.clone();
            motion.connect_motion(move |_, x, _| last_x.set(x));
        }
        area.add_controller(motion);

        // Scroll wheel: up = zoom in, down = zoom out (around the pointer).
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        {
            let state = state.clone();
            let area2 = area.clone();
            let adj = adj.clone();
            let scrollbar = scrollbar.clone();
            let last_x = last_x.clone();
            scroll.connect_scroll(move |_, _dx, dy| {
                if dy != 0.0 {
                    let w = f64::from(area2.width().max(1));
                    let factor = if dy < 0.0 { 1.25 } else { 1.0 / 1.25 };
                    state.borrow_mut().zoom_at(factor, last_x.get(), w);
                    sync_scrollbar(&adj, &scrollbar, &area2, &state);
                }
                gtk::glib::Propagation::Stop
            });
        }
        area.add_controller(scroll);

        // Drag on the waveform always marks a region; a tap without movement
        // places the playhead and clears the selection.
        let drag = gtk::GestureDrag::new();
        let start_x = Rc::new(Cell::new(0.0f64));
        let moved = Rc::new(Cell::new(false));
        {
            let start_x = start_x.clone();
            let moved = moved.clone();
            drag.connect_drag_begin(move |_, x, _| {
                start_x.set(x);
                moved.set(false);
            });
        }
        {
            let state = state.clone();
            let area2 = area.clone();
            let start_x = start_x.clone();
            let moved = moved.clone();
            drag.connect_drag_update(move |_, ox, _| {
                if ox.abs() > CLICK_SLOP {
                    moved.set(true);
                }
                if !moved.get() {
                    return;
                }
                let w = f64::from(area2.width().max(1));
                let mut st = state.borrow_mut();
                let x0 = start_x.get();
                let x1 = x0 + ox;
                let t0 = st.x_to_time(x0.min(x1), w);
                let t1 = st.x_to_time(x0.max(x1), w);
                st.sel = if t1 > t0 { Some((t0, t1)) } else { None };
                drop(st);
                area2.queue_draw();
            });
        }
        {
            let state = state.clone();
            let area2 = area.clone();
            let scale2 = scale.clone();
            let start_x = start_x.clone();
            let moved = moved.clone();
            drag.connect_drag_end(move |_, _, _| {
                if moved.get() {
                    return;
                }
                // A tap: place the playhead here and drop any pending selection.
                let w = f64::from(area2.width().max(1));
                let t = state.borrow().x_to_time(start_x.get(), w);
                state.borrow_mut().sel = None;
                scale2.set_value(t); // updates the playhead + redraws
                area2.queue_draw();
            });
        }
        area.add_controller(drag);

        // Scissors: commit the selection as a cut.
        {
            let state = state.clone();
            let area2 = area.clone();
            scissors.connect_clicked(move |_| {
                let sel = state.borrow().sel;
                if let Some(s) = sel {
                    let mut st = state.borrow_mut();
                    st.cuts.push(s);
                    st.sel = None;
                    drop(st);
                    area2.queue_draw();
                }
            });
        }

        // Zoom buttons (around the view centre).
        {
            let state = state.clone();
            let area2 = area.clone();
            let adj = adj.clone();
            let scrollbar = scrollbar.clone();
            zoom_in.connect_clicked(move |_| {
                let w = f64::from(area2.width().max(1));
                state.borrow_mut().zoom_at(1.5, w / 2.0, w);
                sync_scrollbar(&adj, &scrollbar, &area2, &state);
            });
        }
        {
            let state = state.clone();
            let area2 = area.clone();
            let adj = adj.clone();
            let scrollbar = scrollbar.clone();
            zoom_out.connect_clicked(move |_| {
                let w = f64::from(area2.width().max(1));
                state.borrow_mut().zoom_at(1.0 / 1.5, w / 2.0, w);
                sync_scrollbar(&adj, &scrollbar, &area2, &state);
            });
        }

        // Play from the playhead (uses the main player, starting at the position).
        {
            let sender = sender.clone();
            let state = state.clone();
            let path = path.clone();
            play.connect_clicked(move |_| {
                let ms = (state.borrow().playhead * 1000.0).round() as i64;
                sender.input(Msg::RecordingPlayFrom {
                    path: path.clone(),
                    ms,
                });
            });
        }

        // Reset: drop all cuts, the selection, and the zoom.
        {
            let state = state.clone();
            let area2 = area.clone();
            let adj = adj.clone();
            let scrollbar = scrollbar.clone();
            reset.connect_clicked(move |_| {
                {
                    let mut st = state.borrow_mut();
                    st.cuts.clear();
                    st.sel = None;
                    st.zoom = 1.0;
                    st.view_start = 0.0;
                }
                sync_scrollbar(&adj, &scrollbar, &area2, &state);
            });
        }

        // Save: apply the cuts destructively and leave the page.
        {
            let sender = sender.clone();
            let state = state.clone();
            save.connect_clicked(move |_| {
                let cuts = state.borrow().cuts.clone();
                sender.input(Msg::RecordingApplyCut { id, cuts });
            });
        }

        self.push_subpage(&gettext("Edit recording"), &content);

        // Decode the waveform off-thread, then fill in the peaks and timeline range.
        let (tx, rx) = async_channel::bounded(1);
        let decode_path = path.clone();
        std::thread::spawn(move || {
            let res =
                crate::core::waveform::decode_peaks(std::path::Path::new(&decode_path), BUCKETS);
            let _ = tx.send_blocking(res);
        });
        {
            let state = state.clone();
            let area = area.clone();
            let scale = scale.clone();
            let adj = adj.clone();
            let scrollbar = scrollbar.clone();
            gtk::glib::spawn_future_local(async move {
                if let Ok(Ok((peaks, dur))) = rx.recv().await {
                    {
                        let mut st = state.borrow_mut();
                        st.peaks = peaks;
                        st.duration = dur;
                    }
                    scale.set_range(0.0, dur.max(0.001));
                    sync_scrollbar(&adj, &scrollbar, &area, &state);
                }
            });
        }
    }

    /// Destructively applies the editor's cut ranges to a recording. The decode +
    /// re-encode runs on a background thread (it would otherwise freeze the UI for
    /// seconds); the result arrives as [`Msg::RecordingCutDone`], which overwrites
    /// the DB row and returns to the recordings list.
    pub(crate) fn apply_recording_cut(
        &mut self,
        sender: &ComponentSender<Self>,
        id: i64,
        cuts: Vec<(f64, f64)>,
    ) {
        if cuts.is_empty() {
            self.toast(&gettext("Mark a part to cut first"));
            return;
        }
        let Some(rec) = self
            .streaming
            .recording_items
            .iter()
            .find(|r| r.id == id)
            .cloned()
        else {
            return;
        };
        let src = std::path::PathBuf::from(&rec.path);
        // Album from the embedded tag, artist from the DB row (tag as fallback).
        let tag = crate::core::scanner::read_track(&src).ok();
        let album = tag.as_ref().and_then(|t| t.album.clone());
        let artist = rec
            .artist
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| tag.as_ref().and_then(|t| t.artist.clone()));
        let title = rec.title.clone();
        self.toast(&gettext("Editing the recording …"));

        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let res =
                crate::core::waveform::cut(&src, &cuts, artist.as_deref(), &title, album.as_deref());
            let _ =
                tx.send_blocking(res.map(|r| (r.path.to_string_lossy().into_owned(), r.duration_ms)));
        });
        let sender = sender.clone();
        gtk::glib::spawn_future_local(async move {
            let (path, duration_ms) = match rx.recv().await {
                Ok(Ok((p, d))) => (Some(p), d),
                Ok(Err(e)) => {
                    tracing::warn!("Recording cut failed: {e}");
                    (None, 0)
                }
                Err(_) => (None, 0),
            };
            sender.input(Msg::RecordingCutDone {
                id,
                path,
                duration_ms,
            });
        });
    }
}

/// Paints the waveform for the visible window: kept columns in the accent colour,
/// cut columns dimmed, plus the pending selection overlay and the playhead line.
fn draw_waveform(cr: &gtk::cairo::Context, w: i32, h: i32, st: &EditState) {
    let w = f64::from(w);
    let h = f64::from(h);
    let mid = h / 2.0;

    if st.peaks.is_empty() || st.duration <= 0.0 {
        return;
    }
    let n = st.peaks.len();
    let nf = n as f64;
    let dur = st.duration;
    let in_cut = |t: f64| st.cuts.iter().any(|&(a, b)| t >= a && t < b);

    // On-screen spacing of one column grows with the zoom factor.
    cr.set_line_width((w * st.zoom / nf).max(1.0));

    // Two batched strokes: kept columns (accent), then cut columns (dim red).
    for &(r, g, b, a, cut_pass) in &[
        (0.30, 0.55, 0.95, 0.95, false),
        (0.80, 0.22, 0.22, 0.40, true),
    ] {
        cr.set_source_rgba(r, g, b, a);
        for (i, p) in st.peaks.iter().enumerate() {
            let t = (i as f64 + 0.5) / nf * dur;
            if in_cut(t) != cut_pass {
                continue;
            }
            let x = st.time_to_x(t, w);
            if x < -1.0 || x > w + 1.0 {
                continue; // outside the visible window
            }
            let bar_h = (f64::from(*p) * h * 0.9).max(1.0);
            cr.move_to(x, mid - bar_h / 2.0);
            cr.line_to(x, mid + bar_h / 2.0);
        }
        let _ = cr.stroke();
    }

    // Pending selection overlay (clipped to the visible window).
    if let Some((a, b)) = st.sel {
        let x0 = st.time_to_x(a, w).max(0.0);
        let x1 = st.time_to_x(b, w).min(w);
        if x1 > x0 {
            cr.set_source_rgba(0.95, 0.75, 0.10, 0.25);
            cr.rectangle(x0, 0.0, x1 - x0, h);
            let _ = cr.fill();
        }
    }

    // Playhead (only when inside the visible window).
    let px = st.time_to_x(st.playhead.clamp(0.0, dur), w);
    if px >= 0.0 && px <= w {
        cr.set_source_rgba(0.95, 0.95, 0.95, 0.9);
        cr.set_line_width(2.0);
        cr.move_to(px, 0.0);
        cr.line_to(px, h);
        let _ = cr.stroke();
    }
}
