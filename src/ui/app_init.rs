//! The post-`view_output!()` wiring of the root component, split out of the
//! ~1000-line `init()` for readability. Pure move; `model` is the running
//! `App` (here `self`).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::gettext;
use crate::ui::app::{guarded_resume, save_window_state, section_meta, App, AppWidgets, Msg};

impl App {
    /// Wires up everything that needs the built `widgets`: seek-bar/chapter
    /// hover, scroll restore, the adaptive breakpoint, the icon navigation, and
    /// the window-state restore. The `saved_*` values come from the settings DB
    /// (read in `init()` before the model existed).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_init(
        &mut self,
        widgets: &AppWidgets,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        saved_w: Option<i32>,
        saved_h: Option<i32>,
        saved_max: bool,
        saved_section: Option<String>,
    ) {
        self.nav.view_stack = widgets.view_stack.clone();
        self.nav.nav_view = widgets.nav_view.clone();
        self.nav.split = widgets.split.clone();
        self.mini.seek_scale = widgets.seek_scale.clone();
        self.mini.chapter_label = widgets.chapter_label.clone();
        self.files.source_tabs = widgets.source_tabs.clone();
        self.rebuild_source_tabs();

        // Hover over the seek bar → temporarily show the hovered chapter below the
        // title; on leaving, back to the current chapter (at the
        // playback position). Updates only the label (no view rebuild).
        // A small helper function sets the label from a time value.
        fn show_chapter_at(
            label: &gtk::Label,
            chapters: &std::cell::RefCell<Vec<(i64, String)>>,
            val_ms: i64,
        ) {
            let chaps = chapters.borrow();
            let name = chaps
                .iter()
                .rev()
                .find(|(ms, _)| *ms <= val_ms)
                .map(|(_, n)| n.clone())
                .filter(|n| !n.is_empty());
            match name {
                Some(n) => {
                    label.set_text(&n);
                    label.set_visible(true);
                }
                None => label.set_visible(false),
            }
        }
        {
            let chapters = self.mini.chapters.clone();
            let hovering = self.mini.hovering_seek.clone();
            let scale = widgets.seek_scale.clone();
            let label = widgets.chapter_label.clone();
            let motion = gtk::EventControllerMotion::new();
            {
                let (chapters, scale, label, hovering) = (
                    chapters.clone(),
                    scale.clone(),
                    label.clone(),
                    hovering.clone(),
                );
                motion.connect_motion(move |_, x, _| {
                    if chapters.borrow().is_empty() {
                        return;
                    }
                    let adj = scale.adjustment();
                    let w = scale.width() as f64;
                    let span = adj.upper() - adj.lower();
                    if w <= 0.0 || span <= 0.0 {
                        return;
                    }
                    hovering.set(true);
                    let val = adj.lower() + (x / w).clamp(0.0, 1.0) * span;
                    show_chapter_at(&label, &chapters, val as i64);
                });
            }
            motion.connect_leave(move |_| {
                hovering.set(false);
                // Back to the chapter at the current playback position.
                let pos = scale.adjustment().value() as i64;
                show_chapter_at(&label, &chapters, pos);
            });
            widgets.seek_scale.add_controller(motion);
        }

        // Seek bar: dragging/clicking jumps to the position in the running track.
        // `change-value` fires only on user interaction (not on the
        // programmatic `set_value` of the tick), so there is no tug-of-war.
        {
            let sender = sender.clone();
            widgets.seek_scale.connect_change_value(move |_, _, value| {
                sender.input(Msg::Seek(value as i64));
                gtk::glib::Propagation::Proceed
            });
        }

        // Preserve the scroll position of the overview across navigation:
        // `adw::NavigationView` resets the position to 0 when shown again.
        // Therefore, when returning to the root page, restore the remembered value
        // (slightly delayed, after the re-layout).
        {
            let saved = self.nav.overview_scroll.clone();
            widgets.nav_view.connect_popped(move |nav, _page| {
                // Only when we return to the root overview.
                let is_root = nav
                    .visible_page()
                    .and_then(|p| p.tag())
                    .is_some_and(|t| t == "main");
                if !is_root {
                    return;
                }
                if let Some((sc, value)) = saved.borrow().clone() {
                    // Restore with a short delay (only after the re-layout, which
                    // otherwise resets the scroller to 0); second attempt as
                    // a safeguard against timing fluctuations.
                    for delay in [50u64, 250] {
                        let sc = sc.clone();
                        gtk::glib::timeout_add_local_once(
                            std::time::Duration::from_millis(delay),
                            move || sc.vadjustment().set_value(value),
                        );
                    }
                }
            });
        }

        // Adaptive: only at mobile (narrow) width collapse the sidebar and
        // show the top nav. On the desktop the left sidebar remains initially.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            550.0,
            adw::LengthUnit::Sp,
        ));
        // The desktop spacing between title bar and content is dropped in narrow mode.
        breakpoint.add_setter(
            &widgets.content_overlay,
            "margin-top",
            Some(&0i32.to_value()),
        );
        // The transport bar would otherwise overflow on narrow phones: hide the
        // EQ button there (still reachable via the track's context menu).
        breakpoint.add_setter(&widgets.eq_btn, "visible", Some(&false.to_value()));

        // The sidebar / top-nav / Settings visibility is reconciled in one place
        // from the narrow **and** the nav-hidden state, instead of plain
        // breakpoint setters: when only one menu item is visible the whole
        // navigation is suppressed even on the desktop, and Settings then moves
        // to the title bar. The breakpoint itself only flips the `narrow` flag.
        let apply_chrome: std::rc::Rc<dyn Fn()> = {
            let split = widgets.split.clone();
            // Toggle the scroller (parent of the icon strip), not the strip itself,
            // so a hidden nav leaves no empty scroll area behind.
            let top_nav_scroller = widgets.top_nav_scroller.clone();
            let settings_top = widgets.settings_top_btn.clone();
            let narrow = self.nav.narrow.clone();
            let nav_hidden = self.nav.nav_hidden.clone();
            std::rc::Rc::new(move || {
                let single = nav_hidden.get();
                let narrow = narrow.get();
                // Sidebar gone in narrow mode or when the nav is suppressed.
                let collapsed = narrow || single;
                split.set_collapsed(collapsed);
                split.set_show_sidebar(!collapsed);
                // Top nav only in narrow mode, and never when the nav is hidden.
                top_nav_scroller.set_visible(narrow && !single);
                // Settings sits in the title bar whenever the sidebar is gone.
                settings_top.set_visible(collapsed);
            })
        };
        self.nav.apply_chrome = apply_chrome.clone();
        {
            let narrow = self.nav.narrow.clone();
            let apply = apply_chrome.clone();
            breakpoint.connect_apply(move |_| {
                narrow.set(true);
                apply();
            });
        }
        {
            let narrow = self.nav.narrow.clone();
            let apply = apply_chrome.clone();
            breakpoint.connect_unapply(move |_| {
                narrow.set(false);
                apply();
            });
        }
        root.add_breakpoint(breakpoint);

        // Create the icon-only navigation (sidebar + top) in the **saved
        // order** and couple it to the stack. All buttons
        // are created; hidden menu items are merely invisible.
        self.nav.sidebar_nav = widgets.sidebar_nav.clone();
        self.nav.top_nav = widgets.top_nav.clone();
        let mut nav_buttons: Vec<(&'static str, bool, gtk::ToggleButton)> = Vec::new();
        for (is_sidebar, container) in [
            (true, widgets.sidebar_nav.clone()),
            (false, widgets.top_nav.clone()),
        ] {
            let mut group_leader: Option<gtk::ToggleButton> = None;
            for &name in &self.nav.section_order {
                let Some((label, icon)) = section_meta(name) else {
                    continue;
                };
                let btn = gtk::ToggleButton::builder().build();
                btn.set_visible(!self.nav.hidden_sections.contains(name));
                btn.add_css_class("flat");
                // Highlight the active menu item blue on the icon (CSS `:checked`).
                btn.add_css_class("emilia-nav-btn");
                if is_sidebar {
                    // Desktop sidebar: icon **with label**. A slightly
                    // larger icon (clearly visible, never smaller than the default).
                    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                    let img = gtk::Image::from_icon_name(icon);
                    img.set_pixel_size(22);
                    inner.append(&img);
                    inner.append(&gtk::Label::new(Some(&gettext(label))));
                    btn.set_child(Some(&inner));
                    btn.set_hexpand(true);
                } else {
                    // Mobile top bar: icon only, noticeably larger (≈1.6×) than the
                    // default size – never smaller than now.
                    let img = gtk::Image::from_icon_name(icon);
                    img.set_pixel_size(26);
                    btn.set_child(Some(&img));
                    btn.set_tooltip_text(Some(&gettext(label)));
                }
                match &group_leader {
                    Some(leader) => btn.set_group(Some(leader)),
                    None => group_leader = Some(btn.clone()),
                }
                {
                    let stack = widgets.view_stack.clone();
                    let nav = widgets.nav_view.clone();
                    let sender = sender.clone();
                    btn.connect_clicked(move |b| {
                        if b.is_active() {
                            // If a subpage (artist/album/track detail) is open in the
                            // content area, close it first – otherwise the section
                            // switch would happen hidden behind it.
                            nav.pop_to_tag("main");
                            stack.set_visible_child_name(name);
                            // Click on the menu item = to the start of the section.
                            if name == "files" {
                                sender.input(Msg::FilesGoStart);
                            }
                        }
                    });
                }
                container.append(&btn);
                nav_buttons.push((name, is_sidebar, btn));
            }
        }
        self.nav.nav_buttons = nav_buttons.clone();
        // Apply the initial navigation visibility (hidden sections + the
        // single-item suppression with Settings moved to the title bar).
        self.refresh_nav_visibility();

        // Desktop sidebar: "Settings" at the very bottom – layout/design like
        // the menu items above (icon + label). A stretchable spacer
        // pushes the button to the bottom end.
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        widgets.sidebar_nav.append(&spacer);
        let settings_btn = gtk::Button::builder().build();
        settings_btn.add_css_class("flat");
        settings_btn.set_hexpand(true);
        let settings_inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        settings_inner.append(&gtk::Image::from_icon_name("xsi-view-more-symbolic"));
        settings_inner.append(&gtk::Label::new(Some(&gettext("Settings"))));
        settings_btn.set_child(Some(&settings_inner));
        {
            let sender = sender.clone();
            settings_btn.connect_clicked(move |_| sender.input(Msg::OpenSettings));
        }
        widgets.sidebar_nav.append(&settings_btn);

        // Set the active button to match the visible stack page and show the name
        // of the menu item discreetly as the subtitle of the header.
        let win_title = widgets.win_title.clone();
        let sync_active =
            move |stack: &adw::ViewStack, buttons: &[(&'static str, bool, gtk::ToggleButton)]| {
                let cur = stack.visible_child_name();
                let cur = cur.as_deref().unwrap_or("files");
                for (name, _is_sidebar, btn) in buttons {
                    btn.set_active(*name == cur);
                }
                win_title.set_subtitle(
                    &section_meta(cur)
                        .map(|(l, _)| gettext(l))
                        .unwrap_or_default(),
                );
            };
        // Restore the most recently open navigation item – but not a
        // hidden one. As a fallback, fall to the first visible menu item (in the
        // chosen order).
        let restore = saved_section
            .as_deref()
            .filter(|s| !self.nav.hidden_sections.contains(*s))
            .or_else(|| {
                self.nav
                    .section_order
                    .iter()
                    .copied()
                    .find(|n| !self.nav.hidden_sections.contains(*n))
            });
        if let Some(section) = restore {
            widgets.view_stack.set_visible_child_name(section);
        }
        sync_active(&widgets.view_stack, &nav_buttons);
        {
            let stats_sender = self.stats_page.sender().clone();
            widgets
                .view_stack
                .connect_visible_child_notify(move |stack| {
                    sync_active(stack, &nav_buttons);
                    // Recompute the statistics fresh when opening the section.
                    if stack.visible_child_name().as_deref() == Some("stats") {
                        stats_sender.emit(crate::ui::stats_page::StatsInput::Refresh);
                    }
                });
        }

        // Swipe-to-go-back on the file system page: a horizontal drag to the
        // right navigates back. Implemented as a `GestureDrag` in the **capture**
        // phase so it recognises the horizontal intent *early* and claims the
        // sequence — the previous velocity-based `GestureSwipe` ran in the bubble
        // phase and only fired on release, so it lost the race against a row's
        // tap/long-press and felt coarse and late. We only claim once the motion
        // is clearly rightward-horizontal, leaving vertical drags to the list's
        // scrolling and plain taps to the rows.
        let drag = gtk::GestureDrag::new();
        drag.set_touch_only(false);
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let swipe_claimed = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let swipe_claimed = swipe_claimed.clone();
            drag.connect_drag_begin(move |_, _, _| swipe_claimed.set(false));
        }
        {
            let swipe_claimed = swipe_claimed.clone();
            drag.connect_drag_update(move |g, dx, dy| {
                // Take over as soon as the drag is clearly a rightward swipe, so
                // the gesture responds promptly instead of fighting the tap.
                if !swipe_claimed.get() && dx > 30.0 && dx > dy.abs() * 1.2 {
                    swipe_claimed.set(true);
                    g.set_state(gtk::EventSequenceState::Claimed);
                }
            });
        }
        {
            let sender = sender.clone();
            let swipe_claimed = swipe_claimed.clone();
            drag.connect_drag_end(move |_, dx, dy| {
                if swipe_claimed.get() && dx > 50.0 && dx > dy.abs() * 1.2 {
                    sender.input(Msg::NavUp);
                }
            });
        }
        widgets.files_page.add_controller(drag);

        // Restore the window size and save it on close.
        if let (Some(w), Some(h)) = (saved_w, saved_h) {
            root.set_default_size(w, h);
        }
        if saved_max {
            root.maximize();
        }
        let stack_for_close = widgets.view_stack.clone();
        let close_resume = self.transport.close_resume.clone();
        let close_session = self.transport.close_session.clone();
        root.connect_close_request(move |win| {
            // Save the last listening position (covers the gap to the 5-s save).
            if let Some((path, pos, dur)) = close_resume.borrow().clone() {
                if let Ok(lib) = Library::open() {
                    let _ = lib.set_resume_path(&path, guarded_resume(pos, dur));
                }
            }
            // Save the running listening session as the last event (otherwise the
            // currently playing track would be lost on a hard exit).
            if let Some((path, started_at, played_ms, dur)) = close_session.borrow().clone() {
                if played_ms > 0 {
                    if let Ok(lib) = Library::open() {
                        let _ = lib.log_play(&path, started_at, played_ms, dur, false, None);
                    }
                }
            }
            let section = stack_for_close.visible_child_name();
            save_window_state(
                win.default_width(),
                win.default_height(),
                win.is_maximized(),
                section.as_deref(),
            );
            // Explicitly quit so the process reliably exits when the main window
            // is closed. An idle app already returns from `run()` on its own, but
            // an active background feature (media playback, a running device-sync
            // session, the MPRIS/zbus service) can keep the GApplication held, so
            // the process would linger in the background. Quitting here guarantees
            // a full shutdown in every case.
            if let Some(app) = win.application() {
                app.quit();
            }
            gtk::glib::Propagation::Proceed
        });
    }
}
