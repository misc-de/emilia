//! Recording editor subpage: shows a saved recording as a waveform, lets the
//! user click to place the playhead, drag to mark a region, zoom with the +/-
//! buttons or the scroll wheel, pan the zoomed view with the scrollbar, delete
//! the marked region (scissors) — which immediately recomputes the waveform as a
//! shortened preview — play/pause from the playhead with the timeline staying in
//! sync with the audio, and finally save, which destructively re-encodes the
//! recording without the cut ranges (see [`crate::core::waveform`]).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::i18n::gettext;
use crate::ui::app::{App, EditKind, Msg};

/// Number of waveform columns decoded for the display.
const BUCKETS: usize = 1200;
/// Maximum zoom factor (1.0 = the whole recording fills the width).
const MAX_ZOOM: f64 = 50.0;
/// Pointer movement (px) below which a drag counts as a click.
const CLICK_SLOP: f64 = 4.0;
/// How often the timeline follows the audio while previewing (ms).
const FOLLOW_INTERVAL_MS: u64 = 50;
/// Grace ticks after starting playback before the "still playing?" check kicks
/// in — covers the pipeline preroll (reset → preroll → seek → play).
const PLAY_GUARD_TICKS: u8 = 40;

/// Formats a number of seconds as `m:ss` (negative clamped to zero), for the
/// playhead/total time labels next to the timeline.
fn fmt_secs(secs: f64) -> String {
    let s = secs.max(0.0).round() as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Maps an original-timeline position (seconds) onto the shortened preview
/// timeline by subtracting every cut that lies before it. A position inside a cut
/// collapses to that cut's start. `merged` must be sorted and non-overlapping.
fn orig_to_preview(merged: &[(f64, f64)], t: f64) -> f64 {
    let mut p = t;
    for &(a, b) in merged {
        if t >= b {
            p -= b - a;
        } else if t > a {
            p -= t - a;
            break;
        } else {
            break;
        }
    }
    p.max(0.0)
}

/// Inverse of [`orig_to_preview`]: maps a preview position back onto the original
/// timeline by walking the kept segments between the cuts.
fn preview_to_orig(merged: &[(f64, f64)], p: f64) -> f64 {
    let mut acc = 0.0; // preview coordinate at the current segment's start
    let mut start = 0.0; // original coordinate at the current segment's start
    for &(a, b) in merged {
        let seg = a - start; // kept segment [start, a]
        if p <= acc + seg {
            return start + (p - acc);
        }
        acc += seg;
        start = b;
    }
    start + (p - acc)
}

/// Mutable editor state shared between the drawing area and the controls.
struct EditState {
    /// Per-column peak amplitudes (0.0–1.0) over the *original* timeline; empty
    /// until decoded.
    orig_peaks: Vec<f32>,
    /// Total duration of the original recording in seconds; 0.0 until decoded.
    orig_dur: f64,
    /// Preview waveform: `(preview-time seconds, amplitude)` of the kept columns,
    /// recomputed whenever the cuts change. This is what gets drawn.
    vpeaks: Vec<(f64, f32)>,
    /// Duration of the shortened preview in seconds (original minus the cuts).
    duration: f64,
    /// Committed cuts merged + sorted, in *original* time — used for the preview
    /// mapping and to skip the removed parts during playback.
    merged: Vec<(f64, f64)>,
    /// Pending selection (start, end) in *preview* seconds, before it is cut.
    sel: Option<(f64, f64)>,
    /// Committed cut ranges in *original* seconds, to be removed on save.
    cuts: Vec<(f64, f64)>,
    /// Playhead position in *preview* seconds.
    playhead: f64,
    /// Whether the preview is currently playing (drives the play/pause icon).
    playing: bool,
    /// Grace countdown after starting playback (see [`PLAY_GUARD_TICKS`]).
    play_guard: u8,
    /// Last cut end we seeked to while skipping, to avoid re-issuing the seek
    /// every tick; `-1.0` = not currently skipping.
    last_seek_b: f64,
    /// Horizontal zoom factor (>= 1.0).
    zoom: f64,
    /// Left edge of the visible window, in preview seconds.
    view_start: f64,
}

impl EditState {
    /// Duration (preview seconds) of the currently visible window.
    fn visible(&self) -> f64 {
        self.duration / self.zoom.max(1.0)
    }
    /// Maps a widget x (0..w) to a preview time within the visible window.
    fn x_to_time(&self, x: f64, w: f64) -> f64 {
        if self.duration <= 0.0 || w <= 0.0 {
            return 0.0;
        }
        (self.view_start + (x / w).clamp(0.0, 1.0) * self.visible()).clamp(0.0, self.duration)
    }
    /// Maps a preview time to a widget x within the visible window.
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
    /// Recomputes the shortened preview (merged cuts, preview duration, drawn
    /// columns) from `cuts` + the decoded original peaks. Called after a decode,
    /// a new cut, or a reset.
    fn recompute_view(&mut self) {
        // Normalise + clamp the cuts, then merge overlapping ranges.
        let mut cs: Vec<(f64, f64)> = self
            .cuts
            .iter()
            .map(|&(a, b)| {
                let (a, b) = if a <= b { (a, b) } else { (b, a) };
                (a.clamp(0.0, self.orig_dur), b.clamp(0.0, self.orig_dur))
            })
            .filter(|&(a, b)| b > a)
            .collect();
        cs.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (a, b) in cs {
            if let Some(last) = merged.last_mut() {
                if a <= last.1 {
                    last.1 = last.1.max(b);
                    continue;
                }
            }
            merged.push((a, b));
        }
        let cut_total: f64 = merged.iter().map(|&(a, b)| b - a).sum();
        self.duration = (self.orig_dur - cut_total).max(0.0);
        self.merged = merged;

        // Drop the columns that fall inside a cut; map the rest onto preview time.
        let n = self.orig_peaks.len();
        let nf = n.max(1) as f64;
        let mut v = Vec::with_capacity(n);
        for (i, p) in self.orig_peaks.iter().enumerate() {
            let t = (i as f64 + 0.5) / nf * self.orig_dur;
            if self.merged.iter().any(|&(a, b)| t >= a && t < b) {
                continue;
            }
            v.push((orig_to_preview(&self.merged, t), *p));
        }
        self.vpeaks = v;

        // Keep the view and playhead within the (possibly shorter) preview.
        self.view_start = self
            .view_start
            .min((self.duration - self.visible()).max(0.0));
        self.playhead = self.playhead.min(self.duration);
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

/// Switches the play/pause button back to "play" and updates its tooltip.
fn show_play_icon(btn: &gtk::Button) {
    btn.set_icon_name("media-playback-start-symbolic");
    btn.set_tooltip_text(Some(&gettext("Play from the playhead")));
}

impl App {
    pub(crate) fn open_recording_edit(
        &self,
        sender: &ComponentSender<Self>,
        kind: EditKind,
        id: i64,
    ) {
        // Look up the file path of the item being edited; the editor body itself
        // is generic and only needs the path.
        let path = match kind {
            EditKind::Recording => self
                .streaming
                .recording_items
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.path.clone()),
            EditKind::Memo => self
                .memo
                .memo_items
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.path.clone()),
        };
        let Some(path) = path else {
            return;
        };
        // For memos, the editable name shown at the top of the editor.
        let memo_title = match kind {
            EditKind::Memo => self
                .memo
                .memo_items
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.title.clone()),
            EditKind::Recording => None,
        };
        if !std::path::Path::new(&path).exists() {
            self.toast(&gettext("File not found"));
            return;
        }

        let state = Rc::new(RefCell::new(EditState {
            orig_peaks: Vec::new(),
            orig_dur: 0.0,
            vpeaks: Vec::new(),
            duration: 0.0,
            merged: Vec::new(),
            sel: None,
            cuts: Vec::new(),
            playhead: 0.0,
            playing: false,
            play_guard: 0,
            last_seek_b: -1.0,
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

        // Memo name at the very top: a real text field, read-only until the edit
        // (pencil) button is pressed; the new name is only accepted when the save
        // button (or Enter) is pressed.
        if let Some(title) = memo_title {
            let name_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let entry = gtk::Entry::builder()
                .text(&title)
                .hexpand(true)
                .editable(false)
                .build();
            let edit_btn = gtk::Button::from_icon_name("document-edit-symbolic");
            edit_btn.add_css_class("flat");
            edit_btn.set_valign(gtk::Align::Center);
            edit_btn.set_tooltip_text(Some(&gettext("Rename")));
            let save_btn = gtk::Button::from_icon_name("object-select-symbolic");
            save_btn.add_css_class("flat");
            save_btn.set_valign(gtk::Align::Center);
            save_btn.set_tooltip_text(Some(&gettext("Save name")));
            save_btn.set_visible(false);

            // Pencil → enter edit mode (editable + focused, text selected).
            {
                let (entry, edit_btn2, save_btn2) =
                    (entry.clone(), edit_btn.clone(), save_btn.clone());
                edit_btn.connect_clicked(move |_| {
                    entry.set_editable(true);
                    entry.grab_focus();
                    entry.select_region(0, -1);
                    edit_btn2.set_visible(false);
                    save_btn2.set_visible(true);
                });
            }
            // Save → accept the (non-empty) new name and leave edit mode.
            {
                let (sender, entry, edit_btn2, save_btn2) = (
                    sender.clone(),
                    entry.clone(),
                    edit_btn.clone(),
                    save_btn.clone(),
                );
                save_btn.connect_clicked(move |_| {
                    let t = entry.text().to_string();
                    let t = t.trim();
                    if !t.is_empty() {
                        sender.input(Msg::MemoRename {
                            id,
                            title: t.to_string(),
                        });
                    }
                    entry.set_editable(false);
                    save_btn2.set_visible(false);
                    edit_btn2.set_visible(true);
                });
            }
            // Enter in the field = click save (only while editing).
            {
                let save_btn2 = save_btn.clone();
                entry.connect_activate(move |_| {
                    if save_btn2.is_visible() {
                        save_btn2.emit_clicked();
                    }
                });
            }

            name_row.append(&entry);
            name_row.append(&edit_btn);
            name_row.append(&save_btn);
            content.append(&name_row);
        }

        // Top controls: scissors (left), play/pause (right).
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

        // Timeline (synced with the waveform and the audio): a position picker,
        // flanked by the current playhead time (left) and the total length (right).
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        let time_pos = gtk::Label::new(Some("0:00"));
        time_pos.set_css_classes(&["dim-label", "numeric"]);
        let time_total = gtk::Label::new(Some("0:00"));
        time_total.set_css_classes(&["dim-label", "numeric"]);
        {
            let state = state.clone();
            let area2 = area.clone();
            let time_pos = time_pos.clone();
            scale.connect_value_changed(move |s| {
                state.borrow_mut().playhead = s.value();
                area2.queue_draw();
                time_pos.set_label(&fmt_secs(s.value()));
            });
        }
        let scale_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        scale_row.append(&time_pos);
        scale_row.append(&scale);
        scale_row.append(&time_total);
        content.append(&scale_row);

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

        // Scissors: commit the selection as a cut and recompute the preview.
        {
            let state = state.clone();
            let area2 = area.clone();
            let adj = adj.clone();
            let scrollbar = scrollbar.clone();
            let scale2 = scale.clone();
            let time_total = time_total.clone();
            scissors.connect_clicked(move |_| {
                let sel = state.borrow().sel;
                if let Some((s0, s1)) = sel {
                    {
                        let mut st = state.borrow_mut();
                        // The selection is in preview time → map back to the
                        // original timeline before storing it as a cut.
                        let a = preview_to_orig(&st.merged, s0);
                        let b = preview_to_orig(&st.merged, s1);
                        st.cuts.push((a, b));
                        st.sel = None;
                        st.recompute_view();
                    }
                    let dur = state.borrow().duration;
                    scale2.set_range(0.0, dur.max(0.001));
                    time_total.set_label(&fmt_secs(dur));
                    sync_scrollbar(&adj, &scrollbar, &area2, &state);
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

        // Play / pause from the playhead (through the main player).
        {
            let sender = sender.clone();
            let state = state.clone();
            let path = path.clone();
            let play_btn = play.clone();
            play.connect_clicked(move |_| {
                let mut st = state.borrow_mut();
                if st.playing {
                    // → pause: halt the audio, freeze the playhead.
                    st.playing = false;
                    drop(st);
                    show_play_icon(&play_btn);
                    sender.input(Msg::RecordingPreviewPause);
                } else {
                    // → play: (re)start from the current (preview) playhead,
                    // mapped back onto the original file.
                    st.playing = true;
                    st.play_guard = PLAY_GUARD_TICKS;
                    st.last_seek_b = -1.0;
                    let ms = (preview_to_orig(&st.merged, st.playhead) * 1000.0).round() as i64;
                    drop(st);
                    play_btn.set_icon_name("media-playback-pause-symbolic");
                    play_btn.set_tooltip_text(Some(&gettext("Pause")));
                    sender.input(Msg::RecordingPlayFrom {
                        path: path.clone(),
                        ms,
                    });
                }
            });
        }

        // Reset: drop all cuts (and the zoom) after confirming.
        {
            let state = state.clone();
            let area2 = area.clone();
            let adj = adj.clone();
            let scrollbar = scrollbar.clone();
            let scale2 = scale.clone();
            let time_total = time_total.clone();
            reset.connect_clicked(move |btn| {
                let has_cuts = !state.borrow().cuts.is_empty();
                let apply = {
                    let state = state.clone();
                    let area2 = area2.clone();
                    let adj = adj.clone();
                    let scrollbar = scrollbar.clone();
                    let scale2 = scale2.clone();
                    let time_total = time_total.clone();
                    move || {
                        {
                            let mut st = state.borrow_mut();
                            st.cuts.clear();
                            st.sel = None;
                            st.zoom = 1.0;
                            st.view_start = 0.0;
                            st.recompute_view();
                        }
                        let dur = state.borrow().duration;
                        scale2.set_range(0.0, dur.max(0.001));
                        time_total.set_label(&fmt_secs(dur));
                        sync_scrollbar(&adj, &scrollbar, &area2, &state);
                    }
                };
                if !has_cuts {
                    // Nothing committed yet — just reset the zoom silently.
                    apply();
                    return;
                }
                let dlg = adw::AlertDialog::new(
                    Some(&gettext("Discard all edits?")),
                    Some(&gettext(
                        "This removes every marked cut and restores the full recording.",
                    )),
                );
                dlg.add_response("cancel", &gettext("Cancel"));
                dlg.add_response("ok", &gettext("Discard"));
                dlg.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
                dlg.set_default_response(Some("cancel"));
                dlg.set_close_response("cancel");
                let apply = RefCell::new(Some(apply));
                dlg.connect_response(None, move |_, resp| {
                    if resp == "ok" {
                        if let Some(f) = apply.borrow_mut().take() {
                            f();
                        }
                    }
                });
                dlg.present(Some(btn));
            });
        }

        // Save: confirm overwriting the original, then apply the cuts and leave.
        {
            let sender = sender.clone();
            let state = state.clone();
            let overlay = self.toast_overlay.clone();
            save.connect_clicked(move |_| {
                let cuts = state.borrow().cuts.clone();
                if cuts.is_empty() {
                    // Let the apply path surface the "mark a part first" hint.
                    sender.input(Msg::EditApplyCut { kind, id, cuts });
                    return;
                }
                let dlg = adw::AlertDialog::new(
                    Some(&gettext("Overwrite the original recording?")),
                    Some(&gettext(
                        "Saving replaces the original file with the edited version. \
                         This cannot be undone.",
                    )),
                );
                dlg.add_response("cancel", &gettext("Cancel"));
                dlg.add_response("ok", &gettext("Save"));
                dlg.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
                dlg.set_default_response(Some("cancel"));
                dlg.set_close_response("cancel");
                let sender = sender.clone();
                dlg.connect_response(None, move |_, resp| {
                    if resp == "ok" {
                        sender.input(Msg::EditApplyCut {
                            kind,
                            id,
                            cuts: cuts.clone(),
                        });
                    }
                });
                dlg.present(Some(&overlay));
            });
        }

        let page_title = match kind {
            EditKind::Recording => gettext("Edit recording"),
            EditKind::Memo => gettext("Edit memo"),
        };
        self.push_subpage(&page_title, &content);

        // Follow the audio while previewing: keep the timeline + waveform
        // playhead in sync, skip over committed cuts, and flip back to the play
        // icon when playback ends or is replaced. Self-terminates once the
        // subpage (and thus the drawing area) is gone.
        {
            let state = state.clone();
            let scale = scale.clone();
            let adj = adj.clone();
            let scrollbar = scrollbar.clone();
            let play_btn = play.clone();
            let area_weak = area.downgrade();
            let probe = self.player.probe();
            let want_uri = gtk::glib::filename_to_uri(&path, None)
                .ok()
                .map(|g| g.to_string());
            let sender = sender.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(FOLLOW_INTERVAL_MS), move || {
                let Some(area) = area_weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                if area.root().is_none() {
                    return gtk::glib::ControlFlow::Break; // subpage popped
                }
                if !state.borrow().playing {
                    return gtk::glib::ControlFlow::Continue;
                }

                let ours = match (&want_uri, probe.current_uri()) {
                    (Some(w), Some(c)) => *w == c,
                    _ => false,
                };
                let is_playing = probe.is_playing();

                // Once the start-up grace is over, leave play mode if our track is
                // no longer the one playing (ended externally / replaced / paused
                // from the mini player).
                {
                    let mut st = state.borrow_mut();
                    if st.play_guard > 0 {
                        st.play_guard -= 1;
                    }
                    if (!ours || !is_playing) && st.play_guard == 0 {
                        st.playing = false;
                        st.last_seek_b = -1.0;
                        drop(st);
                        show_play_icon(&play_btn);
                        area.queue_draw();
                        return gtk::glib::ControlFlow::Continue;
                    }
                }
                if !ours {
                    return gtk::glib::ControlFlow::Continue; // preroll: our track not loaded yet
                }
                let Some(ms) = probe.position_ms() else {
                    return gtk::glib::ControlFlow::Continue;
                };
                let t_orig = ms as f64 / 1000.0;

                // Inside a committed cut → jump to its end so the preview matches
                // the saved result (once per cut, guarded by `last_seek_b`).
                let skip_to = {
                    let st = state.borrow();
                    st.merged
                        .iter()
                        .find(|&&(a, b)| t_orig >= a && t_orig < b)
                        .map(|&(_, b)| b)
                };
                if let Some(b) = skip_to {
                    let need = (state.borrow().last_seek_b - b).abs() > 0.05;
                    if need {
                        state.borrow_mut().last_seek_b = b;
                        probe.seek_ms((b * 1000.0).round() as i64);
                    }
                    return gtk::glib::ControlFlow::Continue;
                }
                state.borrow_mut().last_seek_b = -1.0;

                // Map to preview time; stop at the (preview) end.
                let (p, dur, ended) = {
                    let st = state.borrow();
                    let p = orig_to_preview(&st.merged, t_orig);
                    (p, st.duration, p >= st.duration - 0.05)
                };
                if ended {
                    {
                        let mut st = state.borrow_mut();
                        st.playing = false;
                        st.last_seek_b = -1.0;
                    }
                    show_play_icon(&play_btn);
                    sender.input(Msg::RecordingPreviewPause);
                    area.queue_draw();
                    return gtk::glib::ControlFlow::Continue;
                }

                // Keep the playhead inside the visible window when zoomed in.
                {
                    let mut st = state.borrow_mut();
                    let vis = st.visible();
                    if p < st.view_start || p > st.view_start + vis {
                        st.view_start = (p - vis / 2.0).clamp(0.0, (dur - vis).max(0.0));
                    }
                }
                scale.set_value(p); // sets the playhead + redraws
                sync_scrollbar(&adj, &scrollbar, &area, &state);
                gtk::glib::ControlFlow::Continue
            });
        }

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
            let time_total = time_total.clone();
            gtk::glib::spawn_future_local(async move {
                if let Ok(Ok((peaks, dur))) = rx.recv().await {
                    {
                        let mut st = state.borrow_mut();
                        st.orig_peaks = peaks;
                        st.orig_dur = dur;
                        st.recompute_view();
                    }
                    scale.set_range(0.0, dur.max(0.001));
                    time_total.set_label(&fmt_secs(state.borrow().duration));
                    sync_scrollbar(&adj, &scrollbar, &area, &state);
                }
            });
        }
    }

    /// Destructively applies the editor's cut ranges to a recording or memo. The
    /// decode + re-encode runs on a background thread (it would otherwise freeze
    /// the UI for seconds); the result arrives as [`Msg::EditCutDone`], which
    /// overwrites the DB row and returns to the list.
    pub(crate) fn apply_recording_cut(
        &mut self,
        sender: &ComponentSender<Self>,
        kind: EditKind,
        id: i64,
        cuts: Vec<(f64, f64)>,
    ) {
        if cuts.is_empty() {
            self.toast(&gettext("Mark a part to cut first"));
            return;
        }
        // (path, artist, album, title) of the item; memos carry no artist/album.
        let meta = match kind {
            EditKind::Recording => self
                .streaming
                .recording_items
                .iter()
                .find(|r| r.id == id)
                .cloned()
                .map(|rec| {
                    let src = std::path::PathBuf::from(&rec.path);
                    // Album from the embedded tag, artist from the DB row (tag fallback).
                    let tag = crate::core::scanner::read_track(&src).ok();
                    let album = tag.as_ref().and_then(|t| t.album.clone());
                    let artist = rec
                        .artist
                        .clone()
                        .filter(|a| !a.trim().is_empty())
                        .or_else(|| tag.as_ref().and_then(|t| t.artist.clone()));
                    (src, artist, album, rec.title.clone())
                }),
            EditKind::Memo => self
                .memo
                .memo_items
                .iter()
                .find(|m| m.id == id)
                .cloned()
                .map(|m| {
                    (
                        std::path::PathBuf::from(&m.path),
                        None,
                        None,
                        m.title.clone(),
                    )
                }),
        };
        let Some((src, artist, album, title)) = meta else {
            return;
        };
        self.toast(&gettext("Editing …"));

        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let res = crate::core::waveform::cut(
                &src,
                &cuts,
                artist.as_deref(),
                &title,
                album.as_deref(),
            );
            let _ = tx
                .send_blocking(res.map(|r| (r.path.to_string_lossy().into_owned(), r.duration_ms)));
        });
        let sender = sender.clone();
        gtk::glib::spawn_future_local(async move {
            let (path, duration_ms) = match rx.recv().await {
                Ok(Ok((p, d))) => (Some(p), d),
                Ok(Err(e)) => {
                    tracing::warn!("Cut failed: {e}");
                    (None, 0)
                }
                Err(_) => (None, 0),
            };
            sender.input(Msg::EditCutDone {
                kind,
                id,
                path,
                duration_ms,
            });
        });
    }
}

/// Paints the shortened preview waveform for the visible window (the kept columns
/// in the accent colour), plus the pending selection overlay and the playhead.
fn draw_waveform(cr: &gtk::cairo::Context, w: i32, h: i32, st: &EditState) {
    let w = f64::from(w);
    let h = f64::from(h);
    let mid = h / 2.0;

    if st.vpeaks.is_empty() || st.duration <= 0.0 {
        return;
    }
    let n = st.vpeaks.len();

    // On-screen spacing of one column grows with the zoom factor.
    cr.set_line_width((w * st.zoom / n as f64).max(1.0));
    cr.set_source_rgba(0.30, 0.55, 0.95, 0.95);
    for &(t, p) in &st.vpeaks {
        let x = st.time_to_x(t, w);
        if x < -1.0 || x > w + 1.0 {
            continue; // outside the visible window
        }
        let bar_h = (f64::from(p) * h * 0.9).max(1.0);
        cr.move_to(x, mid - bar_h / 2.0);
        cr.line_to(x, mid + bar_h / 2.0);
    }
    let _ = cr.stroke();

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
    let px = st.time_to_x(st.playhead.clamp(0.0, st.duration), w);
    if px >= 0.0 && px <= w {
        cr.set_source_rgba(0.95, 0.95, 0.95, 0.9);
        cr.set_line_width(2.0);
        cr.move_to(px, 0.0);
        cr.line_to(px, h);
        let _ = cr.stroke();
    }
}
