//! Fast scrolling through alphabetically grouped lists: a vertical drag along
//! the right edge of a list no longer scrolls it row by row but shows a letter
//! carousel (0–9, A … Z) and jumps straight to the picked section heading.
//!
//! Only active while the visible list is sorted by name — i.e. while it shows
//! alphabetical headings (see [`crate::ui::app_sort::alpha_header`]). With
//! year headings or no grouping the edge scrolls as before.
//!
//! The headings are found generically: plain lists and galleries draw them with
//! [`crate::ui::app_gallery::section_header_label`] (tagged with
//! [`HEADING_CLASS`]), so a widget-tree walk finds them with their positions.
//! The virtualised `ListView`s realise only a few rows, so they register their
//! heading vector via [`register_list`] instead and are jumped with `scroll_to`.

use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// CSS class on every section heading label (set in `section_header_label`).
pub(crate) const HEADING_CLASS: &str = "emilia-section-heading";

/// Width of the grab zone at the right edge of a list, in px.
const EDGE: f64 = 32.0;
/// Vertical travel before the edge drag takes over (a tap still reaches the row).
const CLAIM_DY: f64 = 6.0;
/// Letters shown above and below the current one in the carousel.
const SPREAD: usize = 2;

type Headers = Rc<RefCell<Option<Vec<String>>>>;

thread_local! {
    static LISTS: RefCell<Vec<(gtk::glib::WeakRef<gtk::ListView>, Headers)>> =
        const { RefCell::new(Vec::new()) };
}

/// Registers the per-row heading vector of a virtualised list (one entry per
/// row, same as its `bind` uses).
pub(crate) fn register_list(view: &gtk::ListView, headers: Headers) {
    LISTS.with(|l| l.borrow_mut().push((view.downgrade(), headers)));
}

fn list_headers(view: &gtk::ListView) -> Option<Headers> {
    LISTS.with(|l| {
        l.borrow()
            .iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(view))
            .map(|(_, h)| h.clone())
    })
}

/// Whether `heading` is one of the alphabetical groups `alpha_header` emits
/// (`0–9`, `#` or a single initial) rather than a year or other heading.
pub(crate) fn is_alpha_heading(heading: &str) -> bool {
    heading == "0–9" || heading == "#" || heading.chars().count() == 1
}

/// Collapses per-row headings into `(heading, first row)` jump targets — but
/// only when every heading is alphabetical and there are at least two.
pub(crate) fn alpha_targets(headers: &[String]) -> Option<Vec<(String, usize)>> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for (i, h) in headers.iter().enumerate() {
        if !is_alpha_heading(h) {
            return None;
        }
        if out.last().is_none_or(|(prev, _)| prev != h) {
            out.push((h.clone(), i));
        }
    }
    (out.len() >= 2).then_some(out)
}

/// Where a pick lands.
#[derive(Clone)]
enum Jump {
    /// Vertical offset in the scrolled content (plain list / gallery).
    Offset(f64),
    /// Row of a virtualised list.
    Row(gtk::ListView, u32),
}

#[derive(Default)]
struct State {
    /// Edge drag recognised and claimed — the carousel is up.
    active: bool,
    /// Drag started inside the edge zone with alphabetical headings present.
    armed: bool,
    start_y: f64,
    targets: Vec<(String, Jump)>,
    current: Option<usize>,
    popup: Option<(gtk::Overlay, gtk::Box)>,
}

/// Installs the edge drag on every vertical `ScrolledWindow` below `root`.
/// Lists without alphabetical headings are unaffected (the drag is declined).
pub(crate) fn install_all(root: &gtk::Widget) {
    if let Some(sc) = root.downcast_ref::<gtk::ScrolledWindow>()
        && sc.vscrollbar_policy() != gtk::PolicyType::Never
    {
        install(sc);
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        install_all(&c);
        child = c.next_sibling();
    }
}

/// Installs the edge drag on one scrolled list.
pub(crate) fn install(scroller: &gtk::ScrolledWindow) {
    let state = Rc::new(RefCell::new(State::default()));
    let drag = gtk::GestureDrag::new();
    // Capture: see the press before the rows / the kinetic scrolling do; we
    // only claim it once the drag clearly goes vertical, so taps pass through.
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    let sc = scroller.downgrade();
    {
        let state = state.clone();
        let sc = sc.clone();
        drag.connect_drag_begin(move |g, x, y| {
            let Some(sc) = sc.upgrade() else { return };
            let targets = if x < sc.width() as f64 - EDGE {
                Vec::new()
            } else {
                collect_targets(&sc)
            };
            let armed = !targets.is_empty();
            *state.borrow_mut() = State {
                armed,
                start_y: y,
                targets,
                ..State::default()
            };
            // Outside the borrow: denying emits `cancel` synchronously.
            if !armed {
                g.set_state(gtk::EventSequenceState::Denied);
            }
        });
    }
    {
        let state = state.clone();
        let sc = sc.clone();
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(sc) = sc.upgrade() else { return };
            let (armed, active) = {
                let st = state.borrow();
                (st.armed, st.active)
            };
            if !armed {
                return;
            }
            if !active {
                if dx.abs() > 2.0 * CLAIM_DY && dx.abs() > dy.abs() {
                    // A sideways swipe (e.g. section switch) — not ours.
                    state.borrow_mut().armed = false;
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                if dy.abs() < CLAIM_DY {
                    return;
                }
                state.borrow_mut().active = true;
                g.set_state(gtk::EventSequenceState::Claimed);
            }
            let mut st = state.borrow_mut();
            let y = st.start_y + dy;
            pick(&sc, &mut st, y);
        });
    }
    let finish = {
        let state = state.clone();
        move || {
            let popup = std::mem::take(&mut *state.borrow_mut()).popup;
            if let Some((overlay, popup)) = popup {
                overlay.remove_overlay(&popup);
            }
        }
    };
    {
        let finish = finish.clone();
        drag.connect_drag_end(move |_, _, _| finish());
    }
    drag.connect_cancel(move |_, _| finish());
    scroller.add_controller(drag);
}

/// Jump targets of the visible list, or empty when it has no alphabetical
/// headings.
fn collect_targets(sc: &gtk::ScrolledWindow) -> Vec<(String, Jump)> {
    let Some(child) = sc.child() else {
        return Vec::new();
    };
    if let Some(view) = child.downcast_ref::<gtk::ListView>() {
        let Some(headers) = list_headers(view) else {
            return Vec::new();
        };
        let headers = headers.borrow();
        return headers
            .as_deref()
            .and_then(alpha_targets)
            .map(|t| {
                t.into_iter()
                    .map(|(h, row)| (h, Jump::Row(view.clone(), row as u32)))
                    .collect()
            })
            .unwrap_or_default();
    }
    let mut labels = Vec::new();
    find_headings(&child, &mut labels);
    let offset = sc.vadjustment().value();
    let mut texts = Vec::with_capacity(labels.len());
    let mut out = Vec::with_capacity(labels.len());
    for label in labels {
        // Bounds = border box, so the heading's CSS padding stays in view too.
        let Some(b) = label.compute_bounds(sc) else {
            continue;
        };
        let top = b.y() as f64 - label.margin_top() as f64 + offset;
        let text = label.text().to_string();
        texts.push(text.clone());
        out.push((text, Jump::Offset(top.max(0.0))));
    }
    // Same rule as the virtualised lists: all alphabetical, at least two.
    if alpha_targets(&texts).is_none() {
        return Vec::new();
    }
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// Mapped (i.e. on the visible page, not filtered out) section headings below
/// `w`, in tree order.
fn find_headings(w: &gtk::Widget, out: &mut Vec<gtk::Label>) {
    if !w.is_mapped() {
        return;
    }
    if w.has_css_class(HEADING_CLASS) {
        if let Some(label) = w.downcast_ref::<gtk::Label>() {
            out.push(label.clone());
        }
        return;
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        find_headings(&c, out);
        child = c.next_sibling();
    }
}

/// Maps the finger position `y` (scroller coordinates) onto a heading — top of
/// the list = first group, bottom = last — jumps there and redraws the carousel.
fn pick(sc: &gtk::ScrolledWindow, st: &mut State, y: f64) {
    let n = st.targets.len();
    let h = (sc.height() as f64).max(1.0);
    let idx = (((y / h).clamp(0.0, 1.0) * n as f64) as usize).min(n - 1);
    if st.current != Some(idx) {
        st.current = Some(idx);
        match &st.targets[idx].1 {
            Jump::Offset(v) => sc.vadjustment().set_value(*v),
            Jump::Row(view, row) => {
                // `scroll_to` only scrolls as far as needed, which leaves a row
                // below the viewport at its bottom edge. From the list end every
                // target lies above, so it lands at the top instead.
                let adj = sc.vadjustment();
                adj.set_value(adj.upper());
                view.scroll_to(*row, gtk::ListScrollFlags::NONE, None);
            }
        }
    }
    show_carousel(sc, st, idx, y);
}

/// Shows (or updates) the letter carousel next to the finger: the picked group
/// big in the middle, its neighbours smaller and fainter above and below.
fn show_carousel(sc: &gtk::ScrolledWindow, st: &mut State, idx: usize, y: f64) {
    if st.popup.is_none() {
        let Some(overlay) = sc
            .ancestor(gtk::Overlay::static_type())
            .and_downcast::<gtk::Overlay>()
        else {
            return;
        };
        let popup = gtk::Box::new(gtk::Orientation::Vertical, 2);
        popup.add_css_class("emilia-fastscroll");
        popup.set_halign(gtk::Align::End);
        popup.set_valign(gtk::Align::Start);
        popup.set_can_target(false);
        for slot in 0..=2 * SPREAD {
            let label = gtk::Label::new(None);
            let class = match slot.abs_diff(SPREAD) {
                0 => "emilia-fs-current",
                1 => "emilia-fs-near",
                _ => "emilia-fs-far",
            };
            label.add_css_class(class);
            popup.append(&label);
        }
        overlay.add_overlay(&popup);
        st.popup = Some((overlay, popup));
    }
    let Some((overlay, popup)) = &st.popup else {
        return;
    };
    let mut child = popup.first_child();
    let mut slot = 0usize;
    while let Some(c) = child {
        if let Some(label) = c.downcast_ref::<gtk::Label>() {
            let text = (idx + slot)
                .checked_sub(SPREAD)
                .and_then(|i| st.targets.get(i))
                .map(|(h, _)| h.as_str());
            // Keep empty slots in the layout so the current letter stays centred.
            label.set_label(text.unwrap_or(" "));
            label.set_opacity(if text.is_some() { 1.0 } else { 0.0 });
        }
        slot += 1;
        child = c.next_sibling();
    }
    // Position: left of the thumb at the right edge, centred on the finger.
    let Some(p) = sc.compute_point(
        overlay,
        &gtk::graphene::Point::new(sc.width() as f32, y as f32),
    ) else {
        return;
    };
    let (_, ph, _, _) = popup.measure(gtk::Orientation::Vertical, -1);
    let max_top = (overlay.height() - ph).max(0);
    let top = (p.y() as i32 - ph / 2).clamp(0, max_top);
    let end = (overlay.width() as f32 - p.x()).max(0.0) as i32 + EDGE as i32 + 24;
    popup.set_margin_top(top);
    popup.set_margin_end(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn targets_take_the_first_row_of_each_letter() {
        let t = alpha_targets(&v(&["0–9", "0–9", "A", "B", "B", "W"])).unwrap();
        let got: Vec<(&str, usize)> = t.iter().map(|(h, i)| (h.as_str(), *i)).collect();
        assert_eq!(got, vec![("0–9", 0), ("A", 2), ("B", 3), ("W", 5)]);
    }

    #[test]
    fn year_or_single_group_headings_disable_the_carousel() {
        assert!(alpha_targets(&v(&["2024", "2024", "2019"])).is_none());
        assert!(alpha_targets(&v(&["A", "2019"])).is_none());
        assert!(alpha_targets(&v(&["A", "A"])).is_none());
        assert!(alpha_targets(&[]).is_none());
    }
}
