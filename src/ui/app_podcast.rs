//! Podcasts: Übersichtsliste, Episoden-Unterseite, Abo-Dialog und der
//! Hintergrund-Abruf der Feeds. Episoden werden direkt gestreamt.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
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
    Some(feed.title)
}

impl App {
    /// Baut die Podcast-Liste neu auf (Titel, Episodenzahl, Aktualisieren, Entfernen).
    pub(crate) fn reload_podcasts(&mut self, sender: &ComponentSender<Self>) {
        self.podcast_items = self.library.podcasts().unwrap_or_default();
        while let Some(child) = self.podcasts_list.first_child() {
            self.podcasts_list.remove(&child);
        }
        for (id, title, _image, count) in self.podcast_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .subtitle(format!("{count} Episoden"))
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("microphone-symbolic"));

            let refresh = gtk::Button::builder()
                .icon_name("view-refresh-symbolic")
                .tooltip_text("Feed aktualisieren")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                refresh.connect_clicked(move |_| sender.input(Msg::PodcastRefresh(id)));
            }
            row.add_suffix(&refresh);

            let del = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Podcast entfernen")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                del.connect_clicked(move |_| sender.input(Msg::PodcastDelete(id)));
            }
            row.add_suffix(&del);

            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(Msg::OpenPodcast(id)));
            }
            self.podcasts_list.append(&row);
        }
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
            .description(format!("{} Episoden", episodes.len()))
            .build();

        if episodes.is_empty() {
            group.add(&adw::ActionRow::builder().title("Keine Episoden").build());
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
        self.push_subpage(&format!("Podcast – {title}"), &content);
    }

    /// Dialog: Feed-Adresse (RSS) eingeben und abonnieren.
    pub(crate) fn open_subscribe_podcast_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::AlertDialog::new(
            Some("Podcast abonnieren"),
            Some("Feed-Adresse (RSS) eingeben:"),
        );
        let entry = gtk::Entry::builder()
            .placeholder_text("https://…/feed.xml")
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Abbrechen"), ("add", "Abonnieren")]);
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
            Err(e) => tracing::error!("Episode konnte nicht abgespielt werden: {e}"),
        }
    }
}
