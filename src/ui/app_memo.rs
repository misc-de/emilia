//! Voice memos page: a "Recent" list of microphone recordings, filterable by a
//! user-created category, with per-memo actions (play, edit via the shared
//! waveform editor, rename, assign category, delete) and category management.
//!
//! Recording itself runs through [`crate::core::mic::MicRecorder`]; the record
//! button lives in the player bar (built in `app.rs`). New memos start
//! uncategorised ("General"); a category is assigned afterwards.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::mic::MicRecorder;
use crate::i18n::gettext;
use crate::model::{MemoCategory, MemoItem};
use crate::ui::app::{fmt_duration, App, Msg};

/// Memo page state, grouped off the `App` god-object (mirrors `StreamingState`).
pub(crate) struct MemoState {
    /// Memos currently shown (already filtered + newest first).
    pub(crate) memo_items: Vec<MemoItem>,
    /// All categories (for the filter bar and the assignment menu).
    pub(crate) categories: Vec<MemoCategory>,
    /// Active filter: `None` = Recent (all), `Some(None)` = General (unassigned),
    /// `Some(Some(id))` = a specific category.
    pub(crate) filter: Option<Option<i64>>,
    pub(crate) memos_list: gtk::ListBox,
    /// Horizontal bar of filter toggle buttons (rebuilt when categories change).
    pub(crate) filter_bar: gtk::Box,
    /// Active microphone recording; `None` when idle.
    pub(crate) recorder: Option<MicRecorder>,
    /// Whether a recording is in progress (drives the `#[watch]` record button
    /// and the elapsed label without borrowing the recorder).
    pub(crate) recording: bool,
    /// Elapsed time of the running recording as `m:ss` (updated on the 1 s tick).
    pub(crate) rec_elapsed: String,
}

impl MemoState {
    /// Initial (empty) state. `memos_list`/`filter_bar` are the widgets bound by
    /// `#[local_ref]` in the view macro.
    pub(crate) fn new(memos_list: gtk::ListBox, filter_bar: gtk::Box) -> Self {
        MemoState {
            memo_items: Vec::new(),
            categories: Vec::new(),
            filter: None,
            memos_list,
            filter_bar,
            recorder: None,
            recording: false,
            rec_elapsed: String::new(),
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

/// Activatable action row with an icon prefix (for the detail dialogs).
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

    /// Player-bar / page record button: start a new recording or stop the
    /// running one.
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
                self.memo.recorder = Some(rec);
                self.memo.recording = true;
                self.memo.rec_elapsed = "0:00".to_string();
                self.toast(&gettext("Recording …"));
            }
            Err(e) => {
                tracing::warn!("Starting the microphone failed: {e}");
                self.toast(&gettext("Microphone not available"));
            }
        }
    }

    /// Stops the recording and finalizes the file off-thread (the EOS wait would
    /// otherwise block the UI); the result arrives as [`Msg::MemoRecordSaved`].
    fn stop_memo_record(&mut self, sender: &ComponentSender<Self>) {
        let Some(rec) = self.memo.recorder.take() else {
            return;
        };
        self.memo.recording = false;
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

    /// Reloads the categories from the DB and rebuilds the filter bar.
    pub(crate) fn reload_memo_categories(&mut self, sender: &ComponentSender<Self>) {
        self.memo.categories = self.library.memo_categories().unwrap_or_default();
        self.rebuild_memo_filter_bar(sender);
    }

    fn rebuild_memo_filter_bar(&self, sender: &ComponentSender<Self>) {
        let bar = &self.memo.filter_bar;
        while let Some(c) = bar.first_child() {
            bar.remove(&c);
        }
        // (label, filter value): Recent, General, then one per category.
        let mut entries: Vec<(String, Option<Option<i64>>)> = vec![
            (gettext("Recent"), None),
            (gettext("General"), Some(None)),
        ];
        for c in &self.memo.categories {
            entries.push((c.name.clone(), Some(Some(c.id))));
        }
        let mut group: Option<gtk::ToggleButton> = None;
        for (label, value) in entries {
            let btn = gtk::ToggleButton::with_label(&label);
            btn.add_css_class("flat");
            if let Some(g) = group.as_ref() {
                btn.set_group(Some(g));
            }
            btn.set_active(self.memo.filter == value);
            {
                let sender = sender.clone();
                btn.connect_toggled(move |b| {
                    if b.is_active() {
                        sender.input(Msg::SetMemoFilter(value));
                    }
                });
            }
            bar.append(&btn);
            if group.is_none() {
                group = Some(btn);
            }
        }
    }

    /// Rebuilds the memo list for the current filter (newest first).
    pub(crate) fn reload_memos(&mut self, sender: &ComponentSender<Self>) {
        self.memo.memo_items = match self.memo.filter {
            None => self.library.memos(),
            Some(cat) => self.library.memos_in_category(cat),
        }
        .unwrap_or_default();

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
        for m in self.memo.memo_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&m.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            let mut sub: Vec<String> = vec![fmt_datetime(m.recorded_at)];
            if let Some(name) = m.category_id.and_then(|cid| {
                self.memo
                    .categories
                    .iter()
                    .find(|c| c.id == cid)
                    .map(|c| c.name.clone())
            }) {
                sub.push(name);
            }
            row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" · ")));
            row.add_prefix(&gtk::Image::from_icon_name("audio-input-microphone-symbolic"));
            if m.duration_ms > 0 {
                let dur = gtk::Label::new(Some(&fmt_duration(m.duration_ms)));
                dur.set_valign(gtk::Align::Center);
                dur.set_css_classes(&["dim-label", "numeric"]);
                row.add_suffix(&dur);
            }
            // Play button (reuses the recording playback path — both are files).
            let play_btn = gtk::Button::from_icon_name("media-playback-start-symbolic");
            play_btn.set_valign(gtk::Align::Center);
            play_btn.add_css_class("flat");
            play_btn.set_tooltip_text(Some(&gettext("Play")));
            {
                let sender = sender.clone();
                let path = m.path.clone();
                play_btn.connect_clicked(move |_| sender.input(Msg::PlayRecording(path.clone())));
            }
            row.add_suffix(&play_btn);
            {
                let sender = sender.clone();
                let path = m.path.clone();
                row.connect_activated(move |_| sender.input(Msg::PlayRecording(path.clone())));
            }
            // Long press → detail dialog.
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                let id = m.id;
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::OpenMemo(id));
                });
            }
            row.add_controller(lp);
            self.memo.memos_list.append(&row);
        }
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
        head.add_prefix(&gtk::Image::from_icon_name("audio-input-microphone-symbolic"));
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
        details.add(&info_row(&gettext("Recorded"), &fmt_datetime(m.recorded_at)));
        if m.duration_ms > 0 {
            details.add(&info_row(&gettext("Length"), &fmt_duration(m.duration_ms)));
        }
        content.append(&details);

        // Category assignment: General + one row per category, current marked.
        let catgrp = adw::PreferencesGroup::builder()
            .title(gettext("Category"))
            .build();
        let mut options: Vec<(Option<i64>, String)> = vec![(None, gettext("General"))];
        for c in &self.memo.categories {
            options.push((Some(c.id), c.name.clone()));
        }
        for (cid, name) in options {
            let r = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&name))
                .activatable(true)
                .build();
            if cid == m.category_id {
                r.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
            }
            let (sender, dialog) = (sender.clone(), dialog.clone());
            r.connect_activated(move |_| {
                sender.input(Msg::MemoSetCategory {
                    id,
                    category_id: cid,
                });
                dialog.close();
            });
            catgrp.add(&r);
        }
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
            let (sender, dialog, root, cur) =
                (sender.clone(), dialog.clone(), root.clone(), m.title.clone());
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

    /// Category management dialog: add, rename, delete user categories.
    pub(crate) fn open_memo_categories(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::Dialog::builder()
            .title(gettext("Categories"))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let add = action_row(&gettext("New category"), "list-add-symbolic");
        {
            let (sender, root) = (sender.clone(), root.clone());
            add.connect_activated(move |_| {
                let sender = sender.clone();
                prompt_text(
                    &root,
                    &gettext("New category"),
                    "",
                    &gettext("Add"),
                    move |name| sender.input(Msg::MemoCategoryAdd(name)),
                );
            });
        }
        let addgrp = adw::PreferencesGroup::new();
        addgrp.add(&add);
        content.append(&addgrp);

        let list = adw::PreferencesGroup::builder()
            .title(gettext("Your categories"))
            .build();
        if self.memo.categories.is_empty() {
            let empty = adw::ActionRow::builder()
                .title(gettext("No categories yet"))
                .build();
            empty.add_css_class("dim-label");
            list.add(&empty);
        }
        for c in &self.memo.categories {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&c.name))
                .build();
            let id = c.id;
            let rename_btn = gtk::Button::from_icon_name("text-editor-symbolic");
            rename_btn.set_valign(gtk::Align::Center);
            rename_btn.add_css_class("flat");
            rename_btn.set_tooltip_text(Some(&gettext("Rename")));
            {
                let (sender, root, cur) = (sender.clone(), root.clone(), c.name.clone());
                rename_btn.connect_clicked(move |_| {
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
            row.add_suffix(&rename_btn);
            let del_btn = gtk::Button::from_icon_name("user-trash-symbolic");
            del_btn.set_valign(gtk::Align::Center);
            del_btn.add_css_class("flat");
            del_btn.set_tooltip_text(Some(&gettext("Delete")));
            {
                let sender = sender.clone();
                let overlay = self.toast_overlay.clone();
                del_btn.connect_clicked(move |_| {
                    crate::ui::app::confirm_destructive(
                        &overlay,
                        &gettext("Delete this category? Its memos move to General."),
                        &gettext("Delete"),
                        sender.clone(),
                        Msg::MemoCategoryDelete(id),
                    );
                });
            }
            row.add_suffix(&del_btn);
            list.add(&row);
        }
        content.append(&list);

        present_dialog(&dialog, &content, root);
    }
}
