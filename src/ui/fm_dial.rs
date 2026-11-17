//! The FM tuning dial: a ruler of the band under a fixed needle, moved by a
//! finger like the wheel of a car radio. Dragging scrolls the band (one
//! channel per [`PX_PER_CHANNEL`]), a tap jumps to the channel under the
//! finger, the scroll wheel steps channel by channel. Channels are reported
//! as they pass (`on_change`, for live tuning) and once more when the finger
//! lifts (`on_done`, to save the frequency).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use relm4::gtk;

use crate::core::fm::{self, FREQ_MAX, FREQ_MIN, STEP};

/// Width of one 100 kHz channel on the ruler.
const PX_PER_CHANNEL: f64 = 14.0;
/// A press that moved less than this is a tap.
const TAP_SLOP: f64 = 6.0;
const HEIGHT: i32 = 76;

type Callback = Rc<RefCell<Option<Box<dyn Fn(u16)>>>>;

#[derive(Clone)]
pub(crate) struct FmDial {
    pub(crate) area: gtk::DrawingArea,
    freq: Rc<Cell<u16>>,
    on_change: Callback,
    on_done: Callback,
}

/// The channel `dx` pixels right of `freq` on the ruler, inside the band.
fn freq_at(freq: u16, dx: f64) -> u16 {
    let channels = (dx / PX_PER_CHANNEL).round() as i32;
    let f = i32::from(fm::snap(freq)) + channels * i32::from(STEP);
    fm::snap(f.clamp(i32::from(FREQ_MIN), i32::from(FREQ_MAX)) as u16)
}

impl FmDial {
    pub(crate) fn new(freq: u16) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(HEIGHT)
            .hexpand(true)
            .can_focus(true)
            .focusable(true)
            .css_classes(["fm-dial"])
            .build();
        area.set_tooltip_text(Some(&crate::i18n::gettext(
            "Drag to tune, tap to jump to a frequency",
        )));
        let dial = Self {
            area,
            freq: Rc::new(Cell::new(fm::snap(freq))),
            on_change: Rc::new(RefCell::new(None)),
            on_done: Rc::new(RefCell::new(None)),
        };
        dial.install_drawing();
        dial.install_drag();
        dial.install_scroll();
        dial.install_keys();
        dial
    }

    pub(crate) fn connect_change(&self, f: impl Fn(u16) + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(f));
    }

    pub(crate) fn connect_done(&self, f: impl Fn(u16) + 'static) {
        *self.on_done.borrow_mut() = Some(Box::new(f));
    }

    /// Shows `freq` without reporting it (set from outside).
    pub(crate) fn set_freq(&self, freq: u16) {
        let freq = fm::snap(freq);
        if self.freq.get() != freq {
            self.freq.set(freq);
            self.area.queue_draw();
        }
    }

    /// Moves to `freq` and reports it, as if the user turned the dial.
    fn turn_to(freq: &Rc<Cell<u16>>, area: &gtk::DrawingArea, cb: &Callback, to: u16) {
        if freq.get() != to {
            freq.set(to);
            area.queue_draw();
            if let Some(f) = cb.borrow().as_ref() {
                f(to);
            }
        }
    }

    fn install_drawing(&self) {
        let freq = self.freq.clone();
        self.area.set_draw_func(move |area, cr, w, h| {
            let fg = area.color();
            let (w, h) = (f64::from(w), f64::from(h));
            let center = w / 2.0;
            let cur = f64::from(freq.get());
            let set = |alpha: f32| {
                cr.set_source_rgba(
                    f64::from(fg.red()),
                    f64::from(fg.green()),
                    f64::from(fg.blue()),
                    f64::from(fg.alpha() * alpha),
                )
            };
            cr.set_line_width(1.5);
            cr.select_font_face(
                "Sans",
                gtk::cairo::FontSlant::Normal,
                gtk::cairo::FontWeight::Normal,
            );
            cr.set_font_size(12.0);
            // Channels visible on either side of the needle.
            let half = (center / PX_PER_CHANNEL).ceil() as i32 + 1;
            let base = i32::from(freq.get());
            for i in -half..=half {
                let ch = base + i * i32::from(STEP);
                if ch < i32::from(FREQ_MIN) || ch > i32::from(FREQ_MAX) {
                    continue;
                }
                let x = center + (f64::from(ch) - cur) / f64::from(STEP) * PX_PER_CHANNEL;
                let (len, alpha) = if ch % 100 == 0 {
                    (h * 0.42, 0.9)
                } else if ch % 50 == 0 {
                    (h * 0.28, 0.6)
                } else {
                    (h * 0.16, 0.35)
                };
                set(alpha);
                cr.move_to(x, h - len);
                cr.line_to(x, h);
                let _ = cr.stroke();
                // Every even MHz labelled (at 14 px per channel: every 280 px).
                if ch % 200 == 0 {
                    let text = (ch / 100).to_string();
                    // Beside the tick, not above it: the needle would cross
                    // the number whenever a full MHz sits under it.
                    if let Ok(ext) = cr.text_extents(&text) {
                        set(0.8);
                        cr.move_to(x + 4.0 - ext.x_bearing(), h - len + ext.height());
                        let _ = cr.show_text(&text);
                    }
                }
            }
            // The needle.
            cr.set_source_rgba(0.88, 0.11, 0.14, 1.0);
            cr.set_line_width(3.0);
            cr.move_to(center, 4.0);
            cr.line_to(center, h);
            let _ = cr.stroke();
        });
    }

    fn install_drag(&self) {
        let drag = gtk::GestureDrag::new();
        let start = Rc::new(Cell::new(self.freq.get()));
        let moved = Rc::new(Cell::new(false));
        {
            let (freq, start, moved) = (self.freq.clone(), start.clone(), moved.clone());
            drag.connect_drag_begin(move |_, _, _| {
                start.set(freq.get());
                moved.set(false);
            });
        }
        {
            let (freq, start, moved, area, cb) = (
                self.freq.clone(),
                start.clone(),
                moved.clone(),
                self.area.clone(),
                self.on_change.clone(),
            );
            drag.connect_drag_update(move |g, dx, dy| {
                if !moved.get() && dx.abs() >= TAP_SLOP && dx.abs() >= dy.abs() {
                    moved.set(true);
                    // A sideways drag is the dial's: the page's scrolling must
                    // not take it over when the finger then wanders a bit.
                    g.set_state(gtk::EventSequenceState::Claimed);
                }
                if moved.get() {
                    // Dragging right pulls lower frequencies under the needle.
                    let to = freq_at(start.get(), -dx);
                    Self::turn_to(&freq, &area, &cb, to);
                }
            });
        }
        {
            let (freq, moved, area, change, done) = (
                self.freq.clone(),
                moved,
                self.area.clone(),
                self.on_change.clone(),
                self.on_done.clone(),
            );
            drag.connect_drag_end(move |g, dx, dy| {
                // A tap — not the end of a vertical swipe that scrolled the page.
                if !moved.get() && dx.abs() < TAP_SLOP && dy.abs() < TAP_SLOP {
                    // A tap: the channel under the finger.
                    if let Some((x, _)) = g.start_point() {
                        let center = f64::from(area.width()) / 2.0;
                        let to = freq_at(freq.get(), x - center);
                        Self::turn_to(&freq, &area, &change, to);
                    }
                }
                area.grab_focus();
                if let Some(f) = done.borrow().as_ref() {
                    f(freq.get());
                }
            });
        }
        self.area.add_controller(drag);
    }

    fn install_scroll(&self) {
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        let (freq, area, change, done) = (
            self.freq.clone(),
            self.area.clone(),
            self.on_change.clone(),
            self.on_done.clone(),
        );
        scroll.connect_scroll(move |_, dx, dy| {
            let d = if dy.abs() > dx.abs() { dy } else { dx };
            if d == 0.0 {
                return gtk::glib::Propagation::Proceed;
            }
            let to = freq_at(freq.get(), d.signum() * PX_PER_CHANNEL);
            Self::turn_to(&freq, &area, &change, to);
            if let Some(f) = done.borrow().as_ref() {
                f(freq.get());
            }
            gtk::glib::Propagation::Stop
        });
        self.area.add_controller(scroll);
    }

    fn install_keys(&self) {
        let keys = gtk::EventControllerKey::new();
        let (freq, area, change, done) = (
            self.freq.clone(),
            self.area.clone(),
            self.on_change.clone(),
            self.on_done.clone(),
        );
        keys.connect_key_pressed(move |_, key, _, _| {
            let channels = match key {
                gtk::gdk::Key::Left | gtk::gdk::Key::Down => -1.0,
                gtk::gdk::Key::Right | gtk::gdk::Key::Up => 1.0,
                gtk::gdk::Key::Page_Down => -10.0,
                gtk::gdk::Key::Page_Up => 10.0,
                _ => return gtk::glib::Propagation::Proceed,
            };
            let to = freq_at(freq.get(), channels * PX_PER_CHANNEL);
            Self::turn_to(&freq, &area, &change, to);
            if let Some(f) = done.borrow().as_ref() {
                f(freq.get());
            }
            gtk::glib::Propagation::Stop
        });
        self.area.add_controller(keys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freq_at_moves_by_whole_channels_inside_the_band() {
        assert_eq!(freq_at(9860, 0.0), 9860);
        assert_eq!(freq_at(9860, PX_PER_CHANNEL), 9870);
        assert_eq!(freq_at(9860, -PX_PER_CHANNEL * 0.4), 9860);
        assert_eq!(freq_at(9860, -PX_PER_CHANNEL * 0.6), 9850);
        assert_eq!(freq_at(9860, PX_PER_CHANNEL * 10.0), 9960);
        // No wrap at the ends: the dial stops there.
        assert_eq!(freq_at(FREQ_MAX, PX_PER_CHANNEL * 5.0), FREQ_MAX);
        assert_eq!(freq_at(FREQ_MIN, -PX_PER_CHANNEL * 5.0), FREQ_MIN);
    }
}
