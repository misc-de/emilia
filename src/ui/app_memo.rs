//! Voice memos page. Two tabs:
//! - **Recent**: every memo, newest first (a flat list).
//! - **Category**: a tree — categories sorted alphanumerically, each an
//!   expander holding the memos assigned to it (plus a "General" node for the
//!   unassigned ones). A "+" in the tab bar adds a category; each category node
//!   carries rename/delete actions.
//!
//! Recording itself runs through [`crate::core::mic::MicRecorder`]; the record
//! button lives in the player bar (built in `app.rs`). New memos start
//! uncategorised ("General"); a category is assigned afterwards.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::mic::MicRecorder;
use crate::i18n::{gettext, ngettext_n};
use crate::model::{MemoCategory, MemoItem};
use crate::ui::app::{fmt_duration, App, MemoView, Msg};

/// Memo page state, grouped off the `App` god-object (mirrors `StreamingState`).
pub(crate) struct MemoState {
    /// All memos (newest first); the Recent list shows these flat and the
    /// Category tree groups them by `category_id`.
    pub(crate) memo_items: Vec<MemoItem>,
    /// All categories (for the tree and the assignment menu).
    pub(crate) categories: Vec<MemoCategory>,
    /// Which tab is shown: Recent or Category.
    pub(crate) view: MemoView,
    pub(crate) memos_list: gtk::ListBox,
    /// Active microphone recording; `None` when idle.
    pub(crate) recorder: Option<MicRecorder>,
    /// Whether a recording is in progress (drives the `#[watch]` player-bar
    /// record button without borrowing the recorder).
    pub(crate) recording: bool,
    /// Live input level (0.0–1.0), smoothed for the recording meter; read by the
    /// meter's draw function, written by the poll timeout while recording.
    pub(crate) rec_level: Rc<Cell<f32>>,
    /// Frame counter giving the meter bars some life even at a steady level.
    rec_frame: Rc<Cell<u32>>,
    /// The recording-level meter (equalizer bars), redrawn each poll tick.
    pub(crate) rec_meter: gtk::DrawingArea,
    /// The running poll timeout while recording; removed on stop.
    rec_tick: Option<gtk::glib::SourceId>,
}

impl MemoState {
    /// Initial (empty) state. `memos_list` and `rec_meter` are the widgets bound
    /// by `#[local_ref]` in the view macro; the meter's draw function is wired up
    /// here against the shared level/frame cells.
    pub(crate) fn new(memos_list: gtk::ListBox, rec_meter: gtk::DrawingArea) -> Self {
        let rec_level = Rc::new(Cell::new(0.0_f32));
        let rec_frame = Rc::new(Cell::new(0_u32));
        {
            let (lvl, frame) = (rec_level.clone(), rec_frame.clone());
            rec_meter.set_draw_func(move |_, cr, w, h| {
                draw_meter(cr, w, h, lvl.get(), frame.get());
            });
        }
        MemoState {
            memo_items: Vec::new(),
            categories: Vec::new(),
            view: MemoView::Recent,
            memos_list,
            recorder: None,
            recording: false,
            rec_level,
            rec_frame,
            rec_meter,
            rec_tick: None,
        }
    }
}

/// Default title for a fresh memo: "Memo DD.MM.YYYY HH:MM" in local time.
pub(crate) fn memo_default_title() -> String {
    let when = gtk::glib::DateTime::now_local()
        .and_then(|d| d.format("%d.%m.%Y %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    if when.is_empty() {
        gettext("Memo")
    } else {
        format!("{} {}", gettext("Memo"), when)
    }
}

/// Unix seconds → "DD.MM.YYYY HH:MM" (local); falls back to the raw value.
fn fmt_datetime(secs: i64) -> String {
    gtk::glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format("%d.%m.%Y %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| secs.to_string())
}

/// Content box for the detail dialogs (uniform margins).
fn detail_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// Activatable action row with an icon prefix (for the detail dialog).
fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

/// Embeds the content scrollably in a dialog with a header bar and shows it.
fn present_dialog(dialog: &adw::Dialog, content: &gtk::Box, root: &adw::ApplicationWindow) {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    dialog.set_content_width(600);
    crate::ui::app_helpers::close_on_click_outside(dialog);
    dialog.present(Some(root));
}

/// Small text-entry prompt (rename / new category). Calls `on_ok` with the
/// trimmed-non-empty input when confirmed.
fn prompt_text(
    root: &adw::ApplicationWindow,
    heading: &str,
    initial: &str,
    ok_label: &str,
    on_ok: impl Fn(String) + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), None);
    let entry = gtk::Entry::builder()
        .text(initial)
        .activates_default(true)
        .build();
    crate::ui::widgets::no_autofocus(&entry);
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", &gettext("Cancel")), ("ok", ok_label)]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, resp| {
        if resp == "ok" {
            let t = entry.text().to_string();
            if !t.trim().is_empty() {
                on_ok(t.trim().to_string());
            }
        }
    });
    dialog.present(Some(root));
}

impl App {
    // ---- Recording ----

    /// Player-bar record button: start a new recording or stop the running one.
    pub(crate) fn toggle_memo_record(&mut self, sender: &ComponentSender<Self>) {
        if self.memo.recorder.is_some() {
            self.stop_memo_record(sender);
        } else {
            self.start_memo_record();
        }
    }

    fn start_memo_record(&mut self) {
        match MicRecorder::start(&crate::core::mic::memos_dir()) {
            Ok(rec) => {
                self.start_rec_meter(rec.level_handle());
                self.memo.recorder = Some(rec);
                self.memo.recording = true;
                self.toast(&gettext("Recording …"));
            }
            Err(e) => {
                tracing::warn!("Starting the microphone failed: {e}");
                self.toast(&gettext("Microphone not available"));
            }
        }
    }

    /// Polls the live mic level (~30 fps) and redraws the meter while recording.
    /// Fast attack / slow release keeps the bars lively without flicker.
    fn start_rec_meter(&mut self, handle: std::sync::Arc<std::sync::atomic::AtomicU32>) {
        self.stop_rec_meter();
        let (level, frame, meter) = (
            self.memo.rec_level.clone(),
            self.memo.rec_frame.clone(),
            self.memo.rec_meter.clone(),
        );
        let id = gtk::glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
            let raw = crate::core::mic::level_from(&handle);
            let prev = level.get();
            // Jump up instantly, ease down gently.
            let smoothed = if raw > prev {
                raw
            } else {
                prev * 0.80 + raw * 0.20
            };
            level.set(smoothed);
            frame.set(frame.get().wrapping_add(1));
            meter.queue_draw();
            gtk::glib::ControlFlow::Continue
        });
        self.memo.rec_tick = Some(id);
    }

    /// Stops the meter poll and clears the level so the next recording starts flat.
    fn stop_rec_meter(&mut self) {
        if let Some(id) = self.memo.rec_tick.take() {
            id.remove();
        }
        self.memo.rec_level.set(0.0);
        self.memo.rec_meter.queue_draw();
    }

    /// Stops the recording and finalizes the file off-thread (the EOS wait would
    /// otherwise block the UI); the result arrives as [`Msg::MemoRecordSaved`].
    fn stop_memo_record(&mut self, sender: &ComponentSender<Self>) {
        let Some(rec) = self.memo.recorder.take() else {
            return;
        };
        self.memo.recording = false;
        self.stop_rec_meter();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(rec.stop());
        });
        let sender = sender.clone();
        gtk::glib::spawn_future_local(async move {
            let (path, duration_ms) = match rx.recv().await {
                Ok(Ok((p, d))) => (Some(p.to_string_lossy().into_owned()), d),
                Ok(Err(e)) => {
                    tracing::warn!("Finalizing the memo failed: {e}");
                    (None, 0)
                }
                Err(_) => (None, 0),
            };
            sender.input(Msg::MemoRecordSaved { path, duration_ms });
        });
    }

    // ---- List + categories ----

    /// Reloads the categories from the DB, then rebuilds the list/tree.
    pub(crate) fn reload_memo_categories(&mut self, sender: &ComponentSender<Self>) {
        self.memo.categories = self.library.memo_categories().unwrap_or_default();
        self.reload_memos(sender);
    }

    /// Rebuilds the memo list for the current view (Recent flat list, or the
    /// Category tree).
    pub(crate) fn reload_memos(&mut self, sender: &ComponentSender<Self>) {
        self.memo.memo_items = self.library.memos().unwrap_or_default();

        // Backfill the playback length for rows stored without one.
        for m in &mut self.memo.memo_items {
            if m.duration_ms <= 0 {
                let ms = crate::core::scanner::duration_secs(std::path::Path::new(&m.path)) as i64
                    * 1000;
                if ms > 0 {
                    let _ = self.library.set_memo_duration(m.id, ms);
                    m.duration_ms = ms;
                }
            }
        }

        while let Some(child) = self.memo.memos_list.first_child() {
            self.memo.memos_list.remove(&child);
        }

        match self.memo.view {
            MemoView::Recent => {
                for m in self.memo.memo_items.clone() {
                    let row = self.build_memo_row(&m, sender, true);
                    self.memo.memos_list.append(&row);
                }
            }
            MemoView::Category => self.build_memo_tree(sender),
        }
    }

    /// Builds the Category tree: a "General" node for unassigned memos (only if
    /// any) followed by the categories sorted alphanumerically, each an expander
    /// of its assigned memos.
    fn build_memo_tree(&self, sender: &ComponentSender<Self>) {
        // General (unassigned) first, if there are any.
        let unassigned: Vec<MemoItem> = self
            .memo
            .memo_items
            .iter()
            .filter(|m| m.category_id.is_none())
            .cloned()
            .collect();
        if !unassigned.is_empty() {
            let exp = adw::ExpanderRow::builder()
                .title(gettext("General"))
                .build();
            exp.add_suffix(&count_label(unassigned.len()));
            for m in &unassigned {
                exp.add_row(&self.build_memo_row(m, sender, false));
            }
            self.memo.memos_list.append(&exp);
        }

        let mut cats = self.memo.categories.clone();
        cats.sort_by_key(|c| c.name.to_lowercase());
        for c in cats {
            let items: Vec<MemoItem> = self
                .memo
                .memo_items
                .iter()
                .filter(|m| m.category_id == Some(c.id))
                .cloned()
                .collect();
            let exp = adw::ExpanderRow::builder()
                .title(gtk::glib::markup_escape_text(&c.name))
                .build();
            exp.add_suffix(&count_label(items.len()));
            for m in &items {
                exp.add_row(&self.build_memo_row(m, sender, false));
            }
            // Long press (touch) / right click (mouse) → category detail dialog.
            // Only real categories carry this; the "General" node above is a label
            // for the unassigned memos and cannot be edited or removed.
            let cid = c.id;
            crate::ui::app::on_secondary_click(&exp, {
                let sender = sender.clone();
                move || sender.input(Msg::OpenMemoCategory(cid))
            });
            crate::ui::app::on_long_press(&exp, {
                let sender = sender.clone();
                move || sender.input(Msg::OpenMemoCategory(cid))
            });
            self.memo.memos_list.append(&exp);
        }
    }

    /// One memo row: title + (date, optional category, duration), a play button,
    /// tap to play, long press for the detail dialog. `show_category` adds the
    /// category name to the subtitle (used in the flat Recent list, redundant in
    /// the tree).
    fn build_memo_row(
        &self,
        m: &MemoItem,
        sender: &ComponentSender<Self>,
        show_category: bool,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&m.title))
            .activatable(true)
            .build();
        // Note: no "emilia-flush" here on purpose — the default row padding gives
        // the prefix icon the same gap from the frame as the suffixes on the right.
        let mut sub: Vec<String> = vec![fmt_datetime(m.recorded_at)];
        if show_category {
            if let Some(name) = m.category_id.and_then(|cid| {
                self.memo
                    .categories
                    .iter()
                    .find(|c| c.id == cid)
                    .map(|c| c.name.clone())
            }) {
                sub.push(name);
            }
        }
        row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" · ")));
        row.add_prefix(&gtk::Image::from_icon_name(
            "audio-input-microphone-symbolic",
        ));
        // Duration + play button grouped so the runtime sits directly to the
        // left of the play button (reuses the recording playback path — both are
        // files).
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.set_valign(gtk::Align::Center);
        if m.duration_ms > 0 {
            let dur = gtk::Label::new(Some(&fmt_duration(m.duration_ms)));
            dur.set_valign(gtk::Align::Center);
            dur.set_css_classes(&["dim-label", "numeric"]);
            controls.append(&dur);
        }
        let play_btn = gtk::Button::from_icon_name("media-playback-start-symbolic");
        play_btn.set_valign(gtk::Align::Center);
        play_btn.add_css_class("flat");
        play_btn.set_tooltip_text(Some(&gettext("Play")));
        {
            let sender = sender.clone();
            let path = m.path.clone();
            play_btn.connect_clicked(move |_| sender.input(Msg::PlayRecording(path.clone())));
        }
        controls.append(&play_btn);
        row.add_suffix(&controls);
        {
            let sender = sender.clone();
            let path = m.path.clone();
            row.connect_activated(move |_| sender.input(Msg::PlayRecording(path.clone())));
        }
        // Long press (touch) / right click (mouse) → detail dialog.
        crate::ui::app::on_secondary_click(&row, {
            let sender = sender.clone();
            let id = m.id;
            move || sender.input(Msg::OpenMemo(id))
        });
        crate::ui::app::on_long_press(&row, {
            let sender = sender.clone();
            let id = m.id;
            move || sender.input(Msg::OpenMemo(id))
        });
        row
    }

    /// "+" in the tab bar: prompt for a new category name.
    pub(crate) fn prompt_new_memo_category(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let sender = sender.clone();
        prompt_text(
            root,
            &gettext("New category"),
            "",
            &gettext("Add"),
            move |name| sender.input(Msg::MemoCategoryAdd(name)),
        );
    }

    /// Detail dialog of a memo: metadata, category assignment, and the
    /// play/edit/rename/delete actions. Reached via long press in the list.
    pub(crate) fn open_memo(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let Some(m) = self.memo.memo_items.iter().find(|m| m.id == id).cloned() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&m.title))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // Header.
        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&m.title))
            .build();
        let cat_name = m
            .category_id
            .and_then(|cid| {
                self.memo
                    .categories
                    .iter()
                    .find(|c| c.id == cid)
                    .map(|c| c.name.clone())
            })
            .unwrap_or_else(|| gettext("General"));
        head.set_subtitle(&gtk::glib::markup_escape_text(&cat_name));
        head.add_prefix(&gtk::Image::from_icon_name(
            "audio-input-microphone-symbolic",
        ));
        info.add(&head);
        content.append(&info);

        // Metadata.
        let details = adw::PreferencesGroup::new();
        let info_row = |label: &str, value: &str| {
            let r = adw::ActionRow::builder().title(label).build();
            r.set_subtitle(&gtk::glib::markup_escape_text(value));
            r.add_css_class("property");
            r
        };
        details.add(&info_row(
            &gettext("Recorded"),
            &fmt_datetime(m.recorded_at),
        ));
        if m.duration_ms > 0 {
            details.add(&info_row(&gettext("Length"), &fmt_duration(m.duration_ms)));
        }
        content.append(&details);

        // Category assignment as a pulldown (AdwComboRow), like the song
        // properties: General + one entry per category, single-select.
        let catgrp = adw::PreferencesGroup::new();
        let mut option_ids: Vec<Option<i64>> = vec![None];
        let mut labels: Vec<String> = vec![gettext("General")];
        for c in &self.memo.categories {
            option_ids.push(Some(c.id));
            labels.push(c.name.clone());
        }
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected = option_ids
            .iter()
            .position(|o| *o == m.category_id)
            .unwrap_or(0) as u32;
        let combo = adw::ComboRow::builder()
            .title(gettext("Category"))
            .model(&gtk::StringList::new(&label_refs))
            .selected(selected)
            .build();
        {
            let sender = sender.clone();
            combo.connect_selected_notify(move |c| {
                let category_id = option_ids.get(c.selected() as usize).copied().flatten();
                sender.input(Msg::MemoSetCategory { id, category_id });
            });
        }
        catgrp.add(&combo);
        content.append(&catgrp);

        // Actions.
        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, path) = (sender.clone(), dialog.clone(), m.path.clone());
            play.connect_activated(move |_| {
                sender.input(Msg::PlayRecording(path.clone()));
                dialog.close();
            });
        }
        actions.add(&play);
        let edit = action_row(&gettext("Edit"), "document-edit-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            edit.connect_activated(move |_| {
                sender.input(Msg::EditMemo(id));
                dialog.close();
            });
        }
        actions.add(&edit);
        let rename = action_row(&gettext("Rename"), "text-editor-symbolic");
        {
            let (sender, dialog, root, cur) = (
                sender.clone(),
                dialog.clone(),
                root.clone(),
                m.title.clone(),
            );
            rename.connect_activated(move |_| {
                dialog.close();
                let sender = sender.clone();
                prompt_text(
                    &root,
                    &gettext("Rename memo"),
                    &cur,
                    &gettext("Rename"),
                    move |title| sender.input(Msg::MemoRename { id, title }),
                );
            });
        }
        actions.add(&rename);
        // Share the memo (audio + title + category) over device sync.
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                sender.input(Msg::ShareItems(crate::core::sync::share::Selection {
                    memos: vec![id],
                    ..Default::default()
                }));
                dialog.close();
            });
        }
        actions.add(&share);
        let remove = action_row(&gettext("Delete memo"), "user-trash-symbolic");
        {
            let sender = sender.clone();
            let (overlay, dialog) = (self.toast_overlay.clone(), dialog.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                crate::ui::app::confirm_destructive(
                    &overlay,
                    &gettext("Delete this memo?"),
                    &gettext("Delete"),
                    sender.clone(),
                    Msg::MemoDelete(id),
                );
            });
        }
        actions.add(&remove);
        content.append(&actions);

        present_dialog(&dialog, &content, root);
    }

    /// Detail dialog of a memo category: metadata (creation date, memo count),
    /// rename, and remove (with the two-step delete flow). Reached via long
    /// press / right click on a category node in the Category tree.
    pub(crate) fn open_memo_category(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let Some(c) = self.memo.categories.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        let count = self
            .memo
            .memo_items
            .iter()
            .filter(|m| m.category_id == Some(id))
            .count();

        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&c.name))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // Header: just the memo count — the category name is already the dialog
        // title, so repeating it as the row's primary text would be redundant.
        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(ngettext_n("{n} memo", "{n} memos", count as u32))
            .build();
        head.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
        info.add(&head);
        content.append(&info);

        // Metadata: creation date (only if known — older rows may lack it).
        if let Some(created) = c.created_at {
            let details = adw::PreferencesGroup::new();
            let row = adw::ActionRow::builder().title(gettext("Created")).build();
            row.set_subtitle(&fmt_datetime(created));
            row.add_css_class("property");
            details.add(&row);
            content.append(&details);
        }

        // Actions: rename + remove.
        let actions = adw::PreferencesGroup::new();
        let rename = action_row(&gettext("Rename"), "text-editor-symbolic");
        {
            let (sender, dialog, root, cur) =
                (sender.clone(), dialog.clone(), root.clone(), c.name.clone());
            rename.connect_activated(move |_| {
                dialog.close();
                let sender = sender.clone();
                prompt_text(
                    &root,
                    &gettext("Rename category"),
                    &cur,
                    &gettext("Rename"),
                    move |name| sender.input(Msg::MemoCategoryRename { id, name }),
                );
            });
        }
        actions.add(&rename);
        let remove = action_row(&gettext("Remove category"), "user-trash-symbolic");
        {
            let (sender, dialog, root) = (sender.clone(), dialog.clone(), root.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                confirm_remove_category(&root, &sender, id, count);
            });
        }
        actions.add(&remove);
        content.append(&actions);

        present_dialog(&dialog, &content, root);
    }
}

/// Two-step delete confirmation for a category. First asks whether to delete at
/// all; if the category still holds memos, a second dialog asks whether the
/// memos should be deleted too or kept (moved to "General").
fn confirm_remove_category(
    root: &adw::ApplicationWindow,
    sender: &ComponentSender<App>,
    id: i64,
    count: usize,
) {
    let first = adw::AlertDialog::new(Some(&gettext("Delete this category?")), None);
    first.add_response("cancel", &gettext("Cancel"));
    first.add_response("ok", &gettext("Delete"));
    first.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    first.set_default_response(Some("cancel"));
    first.set_close_response("cancel");
    let sender = sender.clone();
    let parent = root.clone();
    first.connect_response(None, move |_, resp| {
        if resp != "ok" {
            return;
        }
        // No memos → nothing to ask, just remove the (empty) category.
        if count == 0 {
            sender.input(Msg::MemoCategoryDeleteKeepMemos(id));
            return;
        }
        // Memos present → ask whether to delete them too or keep them (General).
        let body = ngettext_n(
            "The category contains {n} memo. Delete it too, or move it to General?",
            "The category contains {n} memos. Delete them too, or move them to General?",
            count as u32,
        );
        let second = adw::AlertDialog::new(Some(&gettext("Delete the memos too?")), Some(&body));
        second.add_response("cancel", &gettext("Cancel"));
        second.add_response("general", &gettext("Move to General"));
        second.add_response("delete", &gettext("Delete memos"));
        second.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        second.set_default_response(Some("general"));
        second.set_close_response("cancel");
        let sender = sender.clone();
        second.connect_response(None, move |_, resp| match resp {
            "general" => sender.input(Msg::MemoCategoryDeleteKeepMemos(id)),
            "delete" => sender.input(Msg::MemoCategoryDeleteWithMemos(id)),
            _ => {}
        });
        second.present(Some(&parent));
    });
    first.present(Some(root));
}

/// Draws the recording level meter: a small row of equalizer bars whose height
/// follows the live input `level` (0.0–1.0), with a per-bar sine wobble (driven
/// by `frame`) so it stays animated even at a steady volume. Red, to match the
/// recording state. Quiet input collapses the bars to a thin baseline.
fn draw_meter(cr: &gtk::cairo::Context, w: i32, h: i32, level: f32, frame: u32) {
    const BARS: usize = 7;
    const GAP: f64 = 3.0;
    let (w, h) = (w as f64, h as f64);
    let bar_w = ((w - GAP * (BARS as f64 - 1.0)) / BARS as f64).max(1.0);
    let level = level.clamp(0.0, 1.0) as f64;
    for i in 0..BARS {
        // Slow sine offset per bar → an equalizer-like ripple, scaled by level.
        let phase = frame as f64 * 0.30 + i as f64 * 0.9;
        let wobble = 0.55 + 0.45 * phase.sin(); // 0.10 – 1.00
        let mag = (level * wobble).clamp(0.05, 1.0);
        let bar_h = (h * mag).max(2.0);
        let x = i as f64 * (bar_w + GAP);
        let y = (h - bar_h) / 2.0; // vertically centered
        cr.set_source_rgba(0.91, 0.30, 0.26, 0.45 + 0.55 * mag);
        cr.rectangle(x, y, bar_w, bar_h);
        let _ = cr.fill();
    }
}

/// A small dim count label (suffix on a category expander).
fn count_label(n: usize) -> gtk::Label {
    let label = gtk::Label::new(Some(&n.to_string()));
    label.set_valign(gtk::Align::Center);
    label.set_css_classes(&["dim-label", "numeric"]);
    label
}
