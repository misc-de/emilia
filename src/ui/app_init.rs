//! The post-`view_output!()` wiring of the root component, split out of the
//! ~1000-line `init()` for readability. Pure move; `model` is the running
//! `App` (here `self`).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::gettext;
use crate::ui::app::{
    guarded_resume, initial_gallery_columns, online_available, relaunch_for_language_change,
    save_window_state, section_meta, ActiveSource, App, AppWidgets, Cmd, Msg, SortCrit, SECTIONS,
    SORTABLE_SECTIONS,
};

/// The settings/state read from the library DB at startup, before the model
/// exists. Produced by [`App::read_init_state`] and destructured back into
/// locals in `init()`, so the model literal stays untouched.
pub(crate) struct InitState {
    pub music_dir: Option<String>,
    pub root_dir: Option<std::path::PathBuf>,
    pub browse_dir: Option<std::path::PathBuf>,
    pub sources: Vec<crate::model::Source>,
    pub first_run: bool,
    pub saved_w: Option<i32>,
    pub saved_h: Option<i32>,
    pub saved_max: bool,
    pub concert_hint_dismissed: bool,
    pub hidden_sections: std::collections::HashSet<String>,
    pub youtube_enabled: bool,
    pub section_order: Vec<&'static str>,
    pub auto_enrich: bool,
    pub repeat_on: bool,
    pub ui_language: String,
    pub sort: std::collections::HashMap<&'static str, (SortCrit, bool)>,
    pub no_group: std::collections::HashMap<&'static str, bool>,
    pub gallery_view: bool,
    pub gallery_columns: u32,
    pub recording_buffer_minutes: u32,
    pub saved_section: Option<String>,
}

impl App {
    /// Read all persisted startup settings from the library DB into an
    /// [`InitState`]. Pure reads (plus a one-time `setup_complete` backfill);
    /// extracted from `init()`'s prologue.
    pub(crate) fn read_init_state(library: &Library) -> InitState {
        let music_dir = library.get_setting("music_dir").ok().flatten();
        let root_dir = music_dir.as_ref().map(std::path::PathBuf::from);
        // Restore the most recently opened folder – only if it still exists
        // and lies under the start folder; otherwise the start folder itself.
        let browse_dir = library
            .get_setting("browse_dir")
            .ok()
            .flatten()
            .map(std::path::PathBuf::from)
            .filter(|p| root_dir.as_ref().is_some_and(|r| p.starts_with(r)) && p.is_dir())
            .or_else(|| root_dir.clone());

        // Additional music sources (local secondary folder / Nextcloud) for the tabs.
        let sources = library.list_sources().unwrap_or_default();

        // First-run setup: shown once when nothing is configured yet. Existing
        // installations (a music folder or sources already set) are silently
        // marked complete instead, so the assistant never appears for them.
        let setup_done = matches!(
            library
                .get_setting("setup_complete")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        let first_run = !setup_done && music_dir.is_none() && sources.is_empty();
        if !setup_done && !first_run {
            let _ = library.set_setting("setup_complete", "1");
        }

        // Most recently saved window size / maximization.
        let saved_w = library
            .get_setting("win_width")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i32>().ok());
        let saved_h = library
            .get_setting("win_height")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i32>().ok());
        let saved_max = matches!(
            library
                .get_setting("win_maximized")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        // Concert options.
        let concert_hint_dismissed = matches!(
            library
                .get_setting("concert_hint_dismissed")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        // Hidden menu items (comma-separated). The old key
        // "concerts_hidden=1" is still honored.
        let mut hidden_sections: std::collections::HashSet<String> = library
            .get_setting("hidden_sections")
            .ok()
            .flatten()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if matches!(
            library
                .get_setting("concerts_hidden")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        ) {
            hidden_sections.insert("concerts".to_string());
        }
        // YouTube is an opt-in feature (off by default; may be restricted in some
        // countries). The yt-dlp extractor is bundled in the Flatpak. When
        // disabled, hide its section – toggling the setting adds/removes "youtube"
        // from `hidden_sections`.
        let youtube_enabled = matches!(
            library
                .get_setting("youtube_enabled")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        if !youtube_enabled {
            hidden_sections.insert("youtube".to_string());
        }
        // Menu order (comma-separated stack names). Unknown names are
        // discarded, new sections appended at the end in default order – so
        // future menu items appear automatically.
        let mut section_order: Vec<&'static str> = library
            .get_setting("section_order")
            .ok()
            .flatten()
            .map(|s| {
                s.split(',')
                    .filter_map(|name| {
                        SECTIONS
                            .iter()
                            .find(|(n, _, _)| *n == name.trim())
                            .map(|(n, _, _)| *n)
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (name, _, _) in SECTIONS {
            if !section_order.contains(&name) {
                section_order.push(name);
            }
        }
        // Automatic online fetch (default: on; only "0" turns it off).
        let auto_enrich = !matches!(
            library.get_setting("auto_enrich").ok().flatten().as_deref(),
            Some("0")
        );
        // Repeat state (default: off).
        let repeat_on = matches!(
            library.get_setting("repeat").ok().flatten().as_deref(),
            Some("1")
        );
        // Display language (default: system locale). It already took effect
        // at startup in `main` via `i18n::init`; here only for the display in
        // the settings switcher.
        let ui_language = library
            .get_setting("ui_language")
            .ok()
            .flatten()
            .unwrap_or_else(|| "system".to_string());
        // Per-section sort (criterion + direction). Each sortable section keeps
        // its own choice in the settings DB ("sort_<section>" / "..._desc").
        let mut sort: std::collections::HashMap<&'static str, (SortCrit, bool)> =
            std::collections::HashMap::new();
        for &section in SORTABLE_SECTIONS {
            let crit = library
                .get_setting(&format!("sort_{section}"))
                .ok()
                .flatten()
                .map(|s| SortCrit::from_key(&s));
            let desc = matches!(
                library
                    .get_setting(&format!("sort_{section}_desc"))
                    .ok()
                    .flatten()
                    .as_deref(),
                Some("1")
            );
            // Only store a non-default entry, so `sort_for` keeps its fallback.
            if crit.is_some() || desc {
                sort.insert(section, (crit.unwrap_or(SortCrit::Name), desc));
            }
        }
        // Per-section "no grouping" flag ("nogroup_<section>"); default grouped.
        let mut no_group: std::collections::HashMap<&'static str, bool> =
            std::collections::HashMap::new();
        for &section in SORTABLE_SECTIONS {
            if matches!(
                library
                    .get_setting(&format!("nogroup_{section}"))
                    .ok()
                    .flatten()
                    .as_deref(),
                Some("1")
            ) {
                no_group.insert(section, true);
            }
        }
        // Gallery view (default: off) and tiles/row (default: 3 mobile / 4 desktop).
        let gallery_view = matches!(
            library
                .get_setting("gallery_view")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        // Tiles per row (2–8). Initial default depends on the form factor:
        // 3 on phone-sized screens, 4 on the desktop (see `initial_gallery_columns`).
        let gallery_columns = library
            .get_setting("gallery_columns")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(initial_gallery_columns)
            .clamp(2, 8);
        // Timeshift buffer for stations in minutes (default 5, 0 = off, max. 60).
        let recording_buffer_minutes = library
            .get_setting("recording_buffer_minutes")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(5)
            .min(60);
        // Most recently open navigation item (only allow valid section names).
        let saved_section = library
            .get_setting("active_section")
            .ok()
            .flatten()
            .filter(|s| SECTIONS.iter().any(|(name, _, _)| name == s));

        InitState {
            music_dir,
            root_dir,
            browse_dir,
            sources,
            first_run,
            saved_w,
            saved_h,
            saved_max,
            concert_hint_dismissed,
            hidden_sections,
            youtube_enabled,
            section_order,
            auto_enrich,
            repeat_on,
            ui_language,
            sort,
            no_group,
            gallery_view,
            gallery_columns,
            recording_buffer_minutes,
            saved_section,
        }
    }

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
        // Build the sleep-timer popover (presets) onto the header zzz button.
        self.setup_sleep_button(&widgets.sleep_btn, sender);
        // Wire the inline list-filter button + search bar.
        self.setup_inline_filter(widgets, sender);

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

        // The title-bar sort button; its popover is (re)built per section.
        self.nav.sort_btn = widgets.sort_btn.clone();

        // Album & artist lists: section headings driven by the per-row labels
        // filled in `reload_albums_with`/`reload_artists_with` (alphabetical when
        // sorting by name, year strings by date). `None` means no grouping, so
        // every row's header is cleared; otherwise a header is shown whenever a
        // row's label differs from the row above it.
        let header_func =
            |labels: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>| {
                move |row: &gtk::ListBoxRow, _before: Option<&gtk::ListBoxRow>| {
                    let guard = labels.borrow();
                    let Some(labels) = guard.as_ref() else {
                        row.set_header(None::<&gtk::Widget>);
                        return;
                    };
                    let i = row.index();
                    if i < 0 {
                        row.set_header(None::<&gtk::Widget>);
                        return;
                    }
                    let i = i as usize;
                    let cur = labels.get(i);
                    let prev = i.checked_sub(1).and_then(|p| labels.get(p));
                    match cur {
                        Some(cur) if i == 0 || prev != Some(cur) => {
                            row.set_header(Some(&crate::ui::app_gallery::section_header_label(cur)));
                        }
                        _ => row.set_header(None::<&gtk::Widget>),
                    }
                }
            };
        self.libview
            .albums
            .widget()
            .set_header_func(header_func(self.libview.album_headers.clone()));
        self.libview
            .artists
            .widget()
            .set_header_func(header_func(self.libview.artist_headers.clone()));
        // Concert & audiobook entry lists: same alphabetical headings (by name)
        // as the albums; the labels are filled in `load_concerts`/`load_audiobooks`.
        self.concerts
            .concerts_list
            .set_header_func(header_func(self.libview.concert_headers.clone()));
        self.favorites
            .audiobooks_list
            .set_header_func(header_func(self.libview.audiobook_headers.clone()));

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
        // Build the sort popover for the section shown at startup.
        self.rebuild_sort_menu();
        {
            let stats_sender = self.stats_page.sender().clone();
            let sender = sender.clone();
            widgets
                .view_stack
                .connect_visible_child_notify(move |stack| {
                    sync_active(stack, &nav_buttons);
                    // Rebuild (or hide) the title-bar sort control for the section.
                    sender.input(Msg::SortMenuRefresh);
                    // Recompute the statistics fresh when opening the section.
                    if stack.visible_child_name().as_deref() == Some("stats") {
                        stats_sender.emit(crate::ui::stats_page::StatsInput::Refresh);
                    }
                });
        }

        // Shared-header sync: a pushed subpage (album/track list, …) shows a back
        // arrow + the page title in the single header; on the root the back arrow
        // hides and the section name returns as the subtitle. Keeps the top/bottom
        // navigation visible across subpages.
        {
            let win_title = widgets.win_title.clone();
            let back_btn = widgets.nav_back_btn.clone();
            let stack = widgets.view_stack.clone();
            widgets.nav_view.connect_visible_page_notify(move |nv| {
                let on_main = nv
                    .visible_page()
                    .and_then(|p| p.tag())
                    .is_some_and(|t| t == "main");
                back_btn.set_visible(!on_main);
                if on_main {
                    win_title.set_title("Emilia");
                    let cur = stack.visible_child_name();
                    let cur = cur.as_deref().unwrap_or("files");
                    win_title.set_subtitle(
                        &section_meta(cur)
                            .map(|(l, _)| gettext(l))
                            .unwrap_or_default(),
                    );
                } else {
                    let t = nv
                        .visible_page()
                        .map(|p| p.title().to_string())
                        .unwrap_or_default();
                    win_title.set_title(&t);
                    win_title.set_subtitle("");
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

    /// Set the primary music folder: persist it, re-root the file view (only on
    /// the primary tab) and start a background scan.
    pub(crate) fn on_set_music_dir(
        &mut self,
        path: std::path::PathBuf,
        sender: &ComponentSender<Self>,
    ) {
        let dir = path.to_string_lossy().into_owned();
        if let Err(e) = self.library.set_setting("music_dir", &dir) {
            tracing::error!("Failed to save music folder: {e}");
        }
        self.files.music_dir = Some(dir);
        // Only re-root the file view if the primary tab is currently active
        // – on an additional source the user would otherwise be left stranded.
        if self.files.active_source == ActiveSource::Primary {
            self.files.root_dir = Some(path.clone());
            self.files.browse_dir = Some(path);
            self.load_dir(sender);
        }
        // Read the new folder and (Wi-Fi + switch) fetch automatically.
        self.start_scan(sender, true, false);
    }

    /// The first-run setup assistant completed: persist language, music folder
    /// and the enabled menu items, then scan (or relaunch for a language change).
    pub(crate) fn on_setup_finished(
        &mut self,
        lang_code: String,
        music_dir: std::path::PathBuf,
        enabled_sections: Vec<String>,
        sender: &ComponentSender<Self>,
    ) {
        // Which menu items the user keeps. At least one must stay visible.
        let mut enabled: std::collections::HashSet<String> = enabled_sections.into_iter().collect();
        if !SECTIONS.iter().any(|(n, _, _)| enabled.contains(*n)) {
            enabled.insert("files".to_string());
        }
        let hidden_value = SECTIONS
            .iter()
            .map(|(n, _, _)| *n)
            .filter(|n| !enabled.contains(*n))
            .collect::<Vec<_>>()
            .join(",");
        let _ = self.library.set_setting("hidden_sections", &hidden_value);
        // The YouTube section is the opt-in feature: its menu item mirrors
        // the `youtube_enabled` flag.
        let yt_on = enabled.contains("youtube");
        let _ = self
            .library
            .set_setting("youtube_enabled", if yt_on { "1" } else { "0" });
        self.youtube.enabled = yt_on;
        // Persist the rest before any possible restart below.
        let _ = self.library.set_setting("setup_complete", "1");
        let _ = self.library.set_setting("ui_language", &lang_code);
        self.settings.ui_language = lang_code.clone();
        let dir = music_dir.to_string_lossy().into_owned();
        let _ = self.library.set_setting("music_dir", &dir);

        if lang_code != crate::i18n::system_language_code() {
            // The chosen language differs from the active (system) one.
            // gettext only reads the catalog at startup, so relaunch to
            // rebuild the UI in the chosen language; setup is complete now
            // (persisted above), so the assistant won't reappear and the
            // normal startup re-roots the folder and scans.
            relaunch_for_language_change();
        }

        // Same language → keep running: apply the navigation and folder now.
        self.nav.hidden_sections = SECTIONS
            .iter()
            .map(|(n, _, _)| *n)
            .filter(|n| !enabled.contains(*n))
            .map(str::to_string)
            .collect();
        self.refresh_nav_visibility();
        let cur = self.nav.view_stack.visible_child_name();
        let on_hidden = cur
            .as_deref()
            .map(|c| self.nav.hidden_sections.contains(c))
            .unwrap_or(true);
        if on_hidden {
            if let Some(next) = self
                .nav
                .section_order
                .iter()
                .copied()
                .find(|n| !self.nav.hidden_sections.contains(*n))
            {
                self.nav.view_stack.set_visible_child_name(next);
            }
        }
        // Re-root the file view to the chosen folder and start the scan.
        sender.input(Msg::SetMusicDir(music_dir));
    }

    /// The source list changed: reload it, fall back if the active one vanished,
    /// rebuild the tabs and the settings "Other sources" list.
    pub(crate) fn on_sources_changed(&mut self, sender: &ComponentSender<Self>) {
        self.files.sources = self.library.list_sources().unwrap_or_default();
        // If the active source is no longer valid (removed, or the
        // primary "Music" tab dropped because no music folder is set),
        // fall back to the first available folder.
        if let Some(s) = self.active_source_fallback() {
            self.apply_source(s, sender);
        }
        self.rebuild_source_tabs();
        // Indexed cloud tracks may have been added/removed.
        self.reload_library_overviews();
        // Refresh the "Other sources" list of the settings page, in case
        // the settings dialog is currently open (e.g. right after a
        // Nextcloud connect, which lands the source in that same list).
        let src_list = self.settings_src_list.borrow().clone();
        if let Some(list) = src_list {
            if list.root().is_some() {
                self.fill_src_list(&list, sender);
            } else {
                *self.settings_src_list.borrow_mut() = None;
            }
        }
    }

    /// Probe the reachability of all WebDAV sources off-thread.
    pub(crate) fn on_check_sources(&mut self, sender: &ComponentSender<Self>) {
        let webdavs: Vec<crate::model::Source> = self
            .files
            .sources
            .iter()
            .filter(|s| s.kind == "webdav")
            .cloned()
            .collect();
        if !webdavs.is_empty() {
            sender.spawn_command(move |out| {
                let status: Vec<(i64, bool)> = webdavs
                    .iter()
                    .map(|s| {
                        let ok = crate::core::webdav::Creds::from_source(s)
                            .map(|c| crate::core::webdav::test_connection(&c).is_ok())
                            .unwrap_or(false);
                        (s.id, ok)
                    })
                    .collect();
                let _ = out.send(Cmd::SourceStatus(status));
            });
        }
    }

    /// A newly indexed cloud source finished: rebuild the overviews and
    /// (if enabled) fetch covers/photos online.
    pub(crate) fn on_cloud_indexed(&mut self, sender: &ComponentSender<Self>) {
        // Cloud tracks are in the DB → rebuild albums/artists and
        // (if desired) fetch covers/photos online.
        self.reload_library_overviews();
        if self.enrich_state.auto_enrich && !self.enrich_state.enriching && online_available() {
            self.run_enrich(sender, false, false);
        }
    }

    /// Change the display language: persist it and offer to restart now (gettext
    /// only reads the catalog at startup).
    pub(crate) fn on_set_language(&mut self, lang: String, root: &adw::ApplicationWindow) {
        if lang != self.settings.ui_language {
            self.settings.ui_language = lang.clone();
            let _ = self.library.set_setting("ui_language", &lang);
            // gettext reads the language only at startup, so the choice
            // takes effect on the next launch. Ask whether to restart now
            // or later instead of restarting the running app unannounced.
            let confirm = adw::AlertDialog::new(
                Some(&gettext("Restart to change the language?")),
                Some(&gettext(
                    "The new language is loaded only after a restart. Restart now, or do it yourself later.",
                )),
            );
            confirm.add_response("later", &gettext("Later"));
            confirm.add_response("restart", &gettext("Restart now"));
            confirm.set_response_appearance("restart", adw::ResponseAppearance::Suggested);
            confirm.set_default_response(Some("restart"));
            confirm.set_close_response("later");
            confirm.connect_response(None, move |_, resp| {
                if resp == "restart" {
                    relaunch_for_language_change();
                }
            });
            confirm.present(Some(root));
        }
    }

    /// Register the bundled app icons and the application-wide CSS. Runs once at
    /// startup, before the model exists (hence an associated fn, no `self`).
    pub(crate) fn install_styles() {
        // Make custom app icons (e.g. the concert mic) discoverable.
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display)
                .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
            // App icon (logo.png under the app id name) for window/taskbar –
            // takes effect even without an installed .desktop file (e.g. `cargo run`).
            gtk::Window::set_default_icon_name("de.cais.Emilia");

            // Covers/photos in the album/artist list flush left (no indentation).
            let css = gtk::CssProvider::new();
            css.load_from_string(
                "row.emilia-flush > box.header { padding-left: 0px; margin-left: 0px; }\
                 row.emilia-flush > box.header > box.prefixes { margin-left: 0px; margin-right: 8px; }\
                 button.sync-connected { color: @success_color; }\
                 button.sleep-armed { color: @accent_color; }\
                 button.emilia-bigplay, button.emilia-record-dot { min-width: 46px; min-height: 46px; padding: 0px; }\
                 button.emilia-bigplay image, button.emilia-record-dot image { -gtk-icon-size: 34px; }\
                 button.emilia-record-dot image { color: @error_color; }\
                 image.emilia-record-dot { color: @error_color; }\
                 @keyframes emilia-blink { 0% { opacity: 1; } 50% { opacity: 0.25; } 100% { opacity: 1; } }\
                 button.emilia-recording image { animation: emilia-blink 1.1s ease-in-out infinite; }\
                 image.emilia-recording { animation: emilia-blink 1.1s ease-in-out infinite; }\
                 button.emilia-nav-btn:checked image { color: @accent_color; }\
                 box.emilia-step { background-color: alpha(@window_fg_color, 0.12); border-radius: 999px; }\
                 box.emilia-step label { font-weight: bold; }\
                 box.emilia-step-active { background-color: @accent_bg_color; }\
                 box.emilia-step-active label { color: @accent_fg_color; }\
                 scrolledwindow.emilia-nav-scroller scrollbar { opacity: 0; min-width: 0px; min-height: 0px; }\
                 scrolledwindow.emilia-nav-scroller button.emilia-nav-btn { padding-left: 6px; padding-right: 6px; min-width: 0px; }\
                 image.emilia-offline { color: white; background-color: @error_color; border-radius: 999px; padding: 2px; margin: 2px; }\
                 box.emilia-loading { background-color: alpha(@window_bg_color, 0.85); border-radius: 18px; padding: 22px 30px; }\
                 progressbar.emilia-hourbar, progressbar.emilia-hourbar > trough, progressbar.emilia-hourbar > trough > progress { min-width: 0px; }\
                 label.emilia-gallery-title { background-color: alpha(black, 0.55); color: white; padding: 3px 8px; border-bottom-left-radius: 6px; border-bottom-right-radius: 6px; }\
                 flowbox.emilia-gallery > flowboxchild { padding: 0px; border-radius: 6px; }\
                 flowbox.emilia-gallery > flowboxchild:selected { background: none; }\
                 label.emilia-lyric-line { font-size: 1.15em; padding: 5px 4px; transition: color 150ms ease, font-size 150ms ease; }\
                 label.emilia-lyric-active { color: @accent_color; font-weight: bold; font-size: 1.5em; }",
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    }

    /// Worker result: the local library scan finished. Rebuild the views, fill
    /// embedded covers locally and (if enabled) start the online enrichment.
    pub(crate) fn on_cmd_scan_done(
        &mut self,
        then_enrich: bool,
        manual: bool,
        sender: &ComponentSender<Self>,
    ) {
        if manual {
            self.refresh_done();
        }
        // The (initial) scan finished → hide the explanatory loading overlay.
        self.scanning = false;
        // Library is read in → update the views.
        self.reload_library_overviews();
        // Fill in album covers from the embedded artwork in the files —
        // purely local, so they show even offline or with online
        // enrichment disabled (the online sweep below only runs when
        // connected).
        self.run_local_covers(sender);
        // Then automatically fetch online – without user action,
        // provided it is desired, no fetch is already running and there is any
        // connection at all (on any connection, even metered). The
        // local scan already ran, so here without re-reading.
        if then_enrich
            && self.enrich_state.auto_enrich
            && !self.enrich_state.enriching
            && self.files.music_dir.is_some()
            && online_available()
        {
            // Automatic run (without a renewed tag scan), full scope.
            self.run_enrich(sender, false, false);
        }
    }

    /// Worker result: a cloud source finished re-indexing. Rebuild the views and
    /// favorites, then fetch covers/photos (always on a manual refresh).
    pub(crate) fn on_cmd_cloud_reindexed(&mut self, manual: bool, sender: &ComponentSender<Self>) {
        if manual {
            self.refresh_done();
        }
        // Freshly indexed remote tracks → rebuild the library views and
        // favorites. Then fetch covers/photos (incl. the embedded covers
        // of the remote tracks). A manual refresh does this regardless of
        // the passive auto-enrich setting; the silent startup top-up only
        // when auto-enrich is on (like the local scan's `then_enrich`).
        self.reload_library_overviews();
        self.load_favorites(sender);
        if (manual || self.enrich_state.auto_enrich)
            && !self.enrich_state.enriching
            && online_available()
        {
            self.run_enrich(sender, false, false);
        }
    }
}
