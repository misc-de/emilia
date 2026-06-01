//! Podcasts: Übersichtsliste, Episoden-Unterseite, Abo-Dialog und der
//! Hintergrund-Abruf der Feeds. Episoden werden direkt gestreamt.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::{App, Msg};

/// Holt einen Feed und speichert Podcast + Episoden (läuft im Worker-Thread,
/// eigene DB-Verbindung). Gibt bei Erfolg den Podcast-Titel zurück.
pub(crate) fn fetch_and_store_podcast(feed_url: &str) -> Option<String> {
    let feed = crate::core::podcast::fetch_feed(feed_url).ok()?;
    let lib = Library::open().ok()?;
    let id = lib
        .subscribe_podcast(&feed.title, feed_url, feed.image_url.as_deref())
        .ok()?;
    let _ = lib.set_episodes(id, &feed.episodes);
    // Feed-Bild in den Cache laden (Worker-Thread, kein UI-Block).
    if let Some(img) = feed.image_url.as_deref() {
        crate::core::online::cache_podcast_image(img);
    }
    Some(feed.title)
}

/// Inhalts-Box für die Detail-Dialoge (einheitliche Ränder).
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

/// Aktivierbare Aktionszeile mit Icon-Präfix (für die Detail-Dialoge).
fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).activatable(true).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

/// Hängt den Inhalt scrollbar in einen Dialog mit Kopfleiste und zeigt ihn.
fn present_detail(dialog: &adw::Dialog, content: &gtk::Box, root: &adw::ApplicationWindow) {
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
    dialog.present(Some(root));
}

impl App {
    /// Baut die Übersicht der abonnierten Podcasts neu auf: Cover, Titel,
    /// Episodenzahl. Tippen öffnet die Episoden; **langes Drücken** öffnet die
    /// Abo-Detailansicht (Aktualisieren/Entfernen). Aktualisiert anschließend
    /// auch die „Neuste"-Liste.
    pub(crate) fn reload_podcasts(&mut self, sender: &ComponentSender<Self>) {
        self.podcast_items = self.library.podcasts().unwrap_or_default();
        while let Some(child) = self.podcasts_list.first_child() {
            self.podcasts_list.remove(&child);
        }
        for (id, title, image, count) in self.podcast_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .subtitle(ngettext_n("{n} episode", "{n} episodes", count as u32))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            // Cover aus dem RSS-Bild (lokaler Cache); sonst Mikrofon-Platzhalter.
            let cover = image
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            row.add_prefix(&crate::ui::app::cover_widget(
                cover.as_deref(),
                "microphone-symbolic",
            ));
            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(Msg::OpenPodcast(id)));
            }
            // Langes Drücken → Abo-Detailansicht.
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowPodcastDetail(id));
                });
            }
            row.add_controller(lp);
            self.podcasts_list.append(&row);
        }
        self.reload_newest(sender);
    }

    /// Baut die „Neuste"-Liste: neueste Episoden (Beiträge) über **alle** Abos,
    /// chronologisch nach Veröffentlichungsdatum. Tippen streamt; **langes
    /// Drücken** öffnet die Beitrag-Detailansicht.
    pub(crate) fn reload_newest(&mut self, sender: &ComponentSender<Self>) {
        // Nur Episoden aus höchstens ~einem Monat anzeigen.
        let cutoff = crate::core::podcast::recent_cutoff_key();
        let mut eps: Vec<_> = self
            .library
            .all_episodes()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| crate::core::podcast::pubdate_key(e.published.as_deref()) >= cutoff)
            .collect();
        eps.sort_by(|a, b| {
            crate::core::podcast::pubdate_key(b.published.as_deref())
                .cmp(&crate::core::podcast::pubdate_key(a.published.as_deref()))
        });
        eps.truncate(150);
        self.newest_items = eps;
        while let Some(child) = self.newest_list.first_child() {
            self.newest_list.remove(&child);
        }
        for (i, ep) in self.newest_items.iter().enumerate() {
            let mut subtitle = ep.podcast_title.clone();
            if let Some(p) = ep.published.as_deref().filter(|p| !p.trim().is_empty()) {
                subtitle.push_str(" · ");
                subtitle.push_str(&crate::core::podcast::pubdate_short(p));
            }
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&ep.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            let cover = ep
                .podcast_image
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            row.add_prefix(&crate::ui::app::cover_widget(
                cover.as_deref(),
                "microphone-symbolic",
            ));
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::PlayEpisode {
                        url: url.clone(),
                        title: title.clone(),
                    });
                });
            }
            // Langes Drücken → Beitrag-Detailansicht.
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowEpisodeDetail(i));
                });
            }
            row.add_controller(lp);
            self.newest_list.append(&row);
        }
    }

    /// Detailansicht eines Beitrags (Episode) aus der „Neuste"-Liste: Podcast,
    /// Datum, Dauer – mit Aktionen zum Abspielen und zum Öffnen des Podcasts.
    pub(crate) fn open_episode_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        index: usize,
    ) {
        let Some(ep) = self.newest_items.get(index).cloned() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&ep.title))
            .build();
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let pod = adw::ActionRow::builder()
            .title(&gettext("Podcast"))
            .subtitle(gtk::glib::markup_escape_text(&ep.podcast_title))
            .build();
        let cover = ep
            .podcast_image
            .as_deref()
            .and_then(crate::core::online::podcast_image_path);
        pod.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "microphone-symbolic"));
        info.add(&pod);
        if let Some(p) = ep.published.as_deref().filter(|p| !p.trim().is_empty()) {
            info.add(
                &adw::ActionRow::builder()
                    .title(&gettext("Published"))
                    .subtitle(&crate::core::podcast::pubdate_short(p))
                    .build(),
            );
        }
        if let Some(d) = ep.duration.as_deref().filter(|d| !d.trim().is_empty()) {
            info.add(
                &adw::ActionRow::builder()
                    .title(&gettext("Duration"))
                    .subtitle(gtk::glib::markup_escape_text(d.trim()))
                    .build(),
            );
        }
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, url, title) =
                (sender.clone(), dialog.clone(), ep.audio_url.clone(), ep.title.clone());
            play.connect_activated(move |_| {
                sender.input(Msg::PlayEpisode {
                    url: url.clone(),
                    title: title.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&play);
        let open = action_row(&gettext("Open podcast"), "go-next-symbolic");
        {
            let (sender, dialog, pid) = (sender.clone(), dialog.clone(), ep.podcast_id);
            open.connect_activated(move |_| {
                sender.input(Msg::OpenPodcast(pid));
                dialog.close();
            });
        }
        actions.add(&open);
        content.append(&actions);

        present_detail(&dialog, &content, root);
    }

    /// Detailansicht/Verwaltung eines Abos: Cover, Episodenzahl und Aktionen zum
    /// Öffnen, Aktualisieren und Entfernen (mit Rückfrage).
    pub(crate) fn open_podcast_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let Some((_, title, image, count)) =
            self.podcast_items.iter().find(|(p, _, _, _)| *p == id).cloned()
        else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .build();
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .subtitle(ngettext_n("{n} episode", "{n} episodes", count as u32))
            .build();
        let cover = image
            .as_deref()
            .and_then(crate::core::online::podcast_image_path);
        head.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "microphone-symbolic"));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let open = action_row(&gettext("Open episodes"), "go-next-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            open.connect_activated(move |_| {
                sender.input(Msg::OpenPodcast(id));
                dialog.close();
            });
        }
        actions.add(&open);
        let refresh = action_row(&gettext("Refresh feed"), "view-refresh-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            refresh.connect_activated(move |_| {
                sender.input(Msg::PodcastRefresh(id));
                dialog.close();
            });
        }
        actions.add(&refresh);
        let remove = action_row(&gettext("Remove podcast"), "user-trash-symbolic");
        {
            let (sender, dialog, root) = (sender.clone(), dialog.clone(), root.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                crate::ui::app::confirm_destructive(
                    &root,
                    &gettext("Remove this podcast?"),
                    &gettext("Remove"),
                    sender.clone(),
                    Msg::PodcastDelete(id),
                );
            });
        }
        actions.add(&remove);
        content.append(&actions);

        present_detail(&dialog, &content, root);
    }

    /// Episoden-Unterseite eines Podcasts (Tippen = Episode streamen).
    pub(crate) fn open_podcast(&self, sender: &ComponentSender<Self>, id: i64, title: &str) {
        let episodes = self.library.episodes(id).unwrap_or_default();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let group = adw::PreferencesGroup::builder()
            .title(gtk::glib::markup_escape_text(title))
            .description(ngettext_n("{n} episode", "{n} episodes", episodes.len() as u32))
            .build();

        if episodes.is_empty() {
            group.add(&adw::ActionRow::builder().title(&gettext("No episodes")).build());
        }
        for ep in &episodes {
            let mut subtitle = String::new();
            if let Some(p) = &ep.published {
                subtitle.push_str(p.trim());
            }
            if let Some(d) = &ep.duration {
                if !subtitle.is_empty() {
                    subtitle.push_str(" · ");
                }
                subtitle.push_str(d.trim());
            }
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&ep.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("microphone-symbolic"));
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::PlayEpisode {
                        url: url.clone(),
                        title: title.clone(),
                    });
                });
            }
            group.add(&row);
        }
        content.append(&group);
        self.push_subpage(&gettext_f("Podcast – {title}", &[("title", title)]), &content);
    }

    /// Dialog: Feed-Adresse (RSS) eingeben und abonnieren.
    pub(crate) fn open_subscribe_podcast_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::AlertDialog::new(
            Some(&gettext("Subscribe to podcast")),
            Some(&gettext("Enter the feed address (RSS):")),
        );
        let entry = gtk::Entry::builder()
            .placeholder_text("https://…/feed.xml")
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("add", &gettext("Subscribe")),
        ]);
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp == "add" {
                    sender.input(Msg::PodcastSubscribeUrl(entry.text().to_string()));
                }
            });
        }
        dialog.present(Some(root));
    }

    /// Streamt eine Podcast-Episode (ersetzt die laufende Wiedergabe).
    pub(crate) fn play_episode(&mut self, url: &str, title: &str) {
        match self.player.play_uri(url) {
            Ok(()) => {
                self.now_playing = Some(title.to_string());
                self.playing = true;
                self.playing_path = None;
                self.queue.clear();
                self.queue_pos = 0;
                self.position_ms = 0;
                self.track_duration_ms = 0;
                *self.close_resume.borrow_mut() = None;
                self.mpris.set_metadata(0, title, None, None, None, None);
                self.mpris.set_playing(true);
                self.refresh_queue_icons();
            }
            Err(e) => tracing::error!("Failed to play episode: {e}"),
        }
    }
}
