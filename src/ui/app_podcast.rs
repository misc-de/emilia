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

        // Einsortieren nach Aktualität: Heute / Gestern / Diese Woche / Diesen
        // Monat. Die Liste ist absteigend sortiert, daher sind die Abschnitte
        // zusammenhängend; je Abschnitt eine eigene Gruppe (mit Überschrift), und
        // ein Eintrag steht nur im obersten passenden Abschnitt (keine Dopplung).
        let (today, yesterday, week_start) = crate::core::podcast::recent_day_buckets();
        let bucket_of = |k: i64| -> usize {
            if k >= today {
                0
            } else if k >= yesterday {
                1
            } else if k >= week_start {
                2
            } else {
                3
            }
        };
        let bucket_title = |b: usize| match b {
            0 => gettext("Today"),
            1 => gettext("Yesterday"),
            2 => gettext("This week"),
            _ => gettext("This month"),
        };

        let mut cur_bucket: Option<usize> = None;
        let mut group: Option<adw::PreferencesGroup> = None;
        for (i, ep) in self.newest_items.iter().enumerate() {
            let b = bucket_of(crate::core::podcast::pubdate_key(ep.published.as_deref()));
            // Neuer Abschnitt → neue Gruppe mit Überschrift (nur wenn etwas da ist).
            if cur_bucket != Some(b) {
                cur_bucket = Some(b);
                let g = adw::PreferencesGroup::builder().title(&bucket_title(b)).build();
                self.newest_list.append(&g);
                group = Some(g);
            }

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
            row.add_suffix(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::ToggleEpisode {
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
            if let Some(g) = &group {
                g.add(&row);
            }
        }
        // Icons der neu gebauten Zeilen auf den aktuellen Wiedergabestand setzen
        // (und tote Zeilen der vorherigen Liste aussortieren).
        self.refresh_episode_icons();
    }

    /// Detailansicht eines Beitrags (Episode) aus der „Neuste"-Liste: Podcast,
    /// Datum, Dauer – mit Aktionen zum Abspielen und zum Öffnen des Podcasts.
    pub(crate) fn open_episode_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        index: usize,
    ) {
        if let Some(ep) = self.newest_items.get(index).cloned() {
            self.show_episode_detail(root, sender, ep);
        }
    }

    /// Episoden-Detail (inkl. Shownotes) einer Episode aus der Episodenliste
    /// eines geöffneten Podcasts (Index = Reihenfolge in `episodes(id)`).
    pub(crate) fn open_podcast_episode_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        podcast_id: i64,
        index: usize,
    ) {
        let Some(ep) = self
            .library
            .episodes(podcast_id)
            .unwrap_or_default()
            .into_iter()
            .nth(index)
        else {
            return;
        };
        let (podcast_title, podcast_image) = self
            .podcast_items
            .iter()
            .find(|(pid, _, _, _)| *pid == podcast_id)
            .map(|(_, t, img, _)| (t.clone(), img.clone()))
            .unwrap_or_default();
        self.show_episode_detail(
            root,
            sender,
            crate::model::EpisodeRef {
                podcast_id,
                podcast_title,
                podcast_image,
                title: ep.title,
                audio_url: ep.audio_url,
                published: ep.published,
                duration: ep.duration,
                description: ep.description,
            },
        );
    }

    /// Baut den Episoden-Detail-Dialog (geteilt von „Neuste" und der
    /// Episodenliste eines Podcasts): Podcast, Datum, Dauer, Aktionen + Shownotes.
    fn show_episode_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        ep: crate::model::EpisodeRef,
    ) {
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&ep.title))
            .build();
        self.adapt_detail_dialog(&dialog);
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
            // Dauer einheitlich als h:mm:ss (bzw. m:ss) anzeigen.
            let dur = crate::core::podcast::format_duration(d).unwrap_or_else(|| d.trim().to_string());
            info.add(
                &adw::ActionRow::builder()
                    .title(&gettext("Duration"))
                    .subtitle(gtk::glib::markup_escape_text(&dur))
                    .build(),
            );
        }
        content.append(&info);

        // Shownotes (falls vorhanden) direkt unter „Dauer", vor den Aktionen.
        // Zeitstempel (z. B. „12:34") werden zu anklickbaren Sprungmarken.
        if let Some(notes) = ep.description.as_deref().filter(|s| !s.trim().is_empty()) {
            let notes_group = adw::PreferencesGroup::builder()
                .title(&gettext("Shownotes"))
                .build();
            let label = gtk::Label::builder()
                .label(&crate::core::podcast::linkify_timestamps(notes.trim()))
                .use_markup(true)
                .wrap(true)
                .xalign(0.0)
                .selectable(true)
                .build();
            label.add_css_class("body");
            // Klick auf einen Zeitstempel → an die Stelle springen (Episode bei
            // Bedarf dort starten).
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                label.connect_activate_link(move |_, uri| {
                    if let Some(ms) = uri
                        .strip_prefix("emilia-seek:")
                        .and_then(|s| s.parse::<i64>().ok())
                    {
                        sender.input(Msg::EpisodeSeekTo {
                            url: url.clone(),
                            title: title.clone(),
                            ms,
                        });
                        return gtk::glib::Propagation::Stop;
                    }
                    gtk::glib::Propagation::Proceed
                });
            }
            notes_group.add(&label);
            content.append(&notes_group);
        }

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, url, title) =
                (sender.clone(), dialog.clone(), ep.audio_url.clone(), ep.title.clone());
            play.connect_activated(move |_| {
                sender.input(Msg::ToggleEpisode {
                    url: url.clone(),
                    title: title.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&play);
        // „Abspielen" ausblenden, solange genau diese Episode läuft; merken, damit
        // `refresh_episode_icons` die Zeile bei Pause/Ende wieder einblendet.
        let is_current =
            self.playing && self.playing_episode_url.as_deref() == Some(ep.audio_url.as_str());
        play.set_visible(!is_current);
        *self.ctx_episode_play.borrow_mut() = Some((play.clone(), ep.audio_url.clone()));
        {
            let slot = self.ctx_episode_play.clone();
            dialog.connect_closed(move |_| *slot.borrow_mut() = None);
        }
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
        self.adapt_detail_dialog(&dialog);
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
        // Cover des Podcasts einmal ermitteln und in allen Episodenzeilen zeigen.
        let cover = self
            .podcast_items
            .iter()
            .find(|(pid, _, _, _)| *pid == id)
            .and_then(|(_, _, img, _)| img.as_deref())
            .and_then(crate::core::online::podcast_image_path);

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
        for (i, ep) in episodes.iter().enumerate() {
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
            row.add_css_class("emilia-flush");
            row.add_prefix(&crate::ui::app::cover_widget(
                cover.as_deref(),
                "microphone-symbolic",
            ));
            row.add_suffix(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::ToggleEpisode {
                        url: url.clone(),
                        title: title.clone(),
                    });
                });
            }
            // Langes Drücken → Episoden-Detail (inkl. Shownotes).
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowPodcastEpisodeDetail {
                        podcast_id: id,
                        index: i,
                    });
                });
            }
            row.add_controller(lp);
            group.add(&row);
        }
        content.append(&group);
        self.push_subpage(&gettext_f("Podcast – {title}", &[("title", title)]), &content);
        // Icons auf den aktuellen Wiedergabestand setzen.
        self.refresh_episode_icons();
    }

    /// Dialog zum Abonnieren: oben eine **Suche** (durchsucht das iTunes-
    /// Podcast-Verzeichnis und zeigt antippbare Treffer), darunter ein Feld für
    /// die **Feed-Adresse** (RSS) als manueller Weg. Beides führt am Ende über
    /// [`Msg::PodcastSubscribeUrl`] zum üblichen Abo-Abruf.
    pub(crate) fn open_subscribe_podcast_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::Dialog::builder()
            .title(&gettext("Subscribe to podcast"))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // --- Suche (iTunes-Verzeichnis) ---
        let search_group = adw::PreferencesGroup::builder()
            .title(&gettext("Search"))
            .description(&gettext("Find a podcast by name"))
            .build();
        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(&gettext("Podcast name …"))
            .hexpand(true)
            .build();
        let search_btn = gtk::Button::builder().label(&gettext("Search")).build();
        search_btn.add_css_class("suggested-action");
        search_row.append(&search_entry);
        search_row.append(&search_btn);
        search_group.add(&search_row);
        content.append(&search_group);

        // Enter im Suchfeld oder Klick auf „Suchen" startet die Suche.
        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_entry.connect_activate(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(Msg::PodcastSearch(term));
                }
            });
        }
        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_btn.connect_clicked(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(Msg::PodcastSearch(term));
                }
            });
        }

        // Trefferliste – anfangs leer/versteckt, asynchron befüllt von
        // `rebuild_podcast_search_results`.
        let results = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        results.add_css_class("boxed-list");
        results.set_visible(false);
        content.append(&results);

        // --- Manuell: Feed-Adresse (RSS) ---
        let url_group = adw::PreferencesGroup::builder()
            .title(&gettext("Or enter feed address"))
            .build();
        let url_entry = adw::EntryRow::builder()
            .title(&gettext("Feed address (RSS)"))
            .show_apply_button(true)
            .build();
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            url_entry.connect_apply(move |e| {
                let url = e.text().to_string();
                if !url.trim().is_empty() {
                    sender.input(Msg::PodcastSubscribeUrl(url));
                    dialog.close();
                }
            });
        }
        url_group.add(&url_entry);
        content.append(&url_group);

        // Dialog + Trefferliste hinterlegen, damit eintreffende Treffer in die
        // offene Liste gezeichnet werden; beim Schließen wieder freigeben.
        *self.podcast_search.borrow_mut() = Some((dialog.clone(), results.clone()));
        {
            let slot = self.podcast_search.clone();
            dialog.connect_closed(move |_| {
                *slot.borrow_mut() = None;
            });
        }

        present_detail(&dialog, &content, root);
        search_entry.grab_focus();
    }

    /// Zeichnet die Trefferliste im offenen Abo-Such-Dialog neu (aus
    /// `self.podcast_search_results`). Tut nichts, wenn der Dialog zu ist. Jeder
    /// Treffer ist antippbar: Tippen abonniert über die Feed-Adresse und schließt
    /// den Dialog. Cover stammen aus dem lokalen Cache (sonst Mikrofon-Platzhalter).
    pub(crate) fn rebuild_podcast_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.podcast_search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.podcast_search_results.is_empty() {
            let row = adw::ActionRow::builder().title(&gettext("No podcasts found")).build();
            row.set_sensitive(false);
            list.append(&row);
            // Knappe Höhe – nur Such- und Adressfeld plus Hinweiszeile.
            dialog.set_content_height(300);
            return;
        }

        // Dialog so hoch machen, wie die Treffer es brauchen (gedeckelt, dann
        // scrollt die Liste). Grob: feste Bereiche (Kopf, Suche, Adresse) +
        // ~66 px je Trefferzeile.
        let rows = self.podcast_search_results.len() as i32;
        dialog.set_content_height((320 + rows * 66).min(760));

        for r in &self.podcast_search_results {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .activatable(true)
                .build();
            if let Some(a) = r.author.as_deref().filter(|a| !a.trim().is_empty()) {
                row.set_subtitle(&gtk::glib::markup_escape_text(a));
            }
            let cover = r.image_url.as_deref().and_then(crate::core::online::podcast_image_path);
            row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "microphone-symbolic"));
            row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
            {
                let (sender, dialog, feed) =
                    (sender.clone(), dialog.clone(), r.feed_url.clone());
                row.connect_activated(move |_| {
                    sender.input(Msg::PodcastSubscribeUrl(feed.clone()));
                    dialog.close();
                });
            }
            list.append(&row);
        }
    }

    /// Play/Pause-Knopf (Suffix) für eine Beitragszeile: tippt = Episode
    /// umschalten. Wird in `episode_play_buttons` registriert, damit sein Icon
    /// beim Wechsel des Wiedergabestands aktualisiert werden kann.
    fn episode_play_button(
        &self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
    ) -> gtk::Button {
        let btn = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(&gettext("Play/Pause"))
            .build();
        btn.add_css_class("flat");
        {
            let (sender, url, title) = (sender.clone(), url.to_string(), title.to_string());
            btn.connect_clicked(move |_| {
                sender.input(Msg::ToggleEpisode {
                    url: url.clone(),
                    title: title.clone(),
                });
            });
        }
        self.episode_play_buttons
            .borrow_mut()
            .push((url.to_string(), btn.clone()));
        btn
    }

    /// Aktualisiert die Play/Pause-Icons aller sichtbaren Beitragszeilen und die
    /// „Abspielen"-Zeile eines offenen Detaildialogs. Abgehängte Zeilen (z. B.
    /// nach Verlassen einer Unterseite) werden dabei aussortiert.
    pub(crate) fn refresh_episode_icons(&self) {
        let active = self.playing_episode_url.clone();
        let playing = self.playing;
        let is_active = |url: &str| playing && active.as_deref() == Some(url);
        {
            let mut buttons = self.episode_play_buttons.borrow_mut();
            buttons.retain(|(_, btn)| btn.root().is_some());
            for (url, btn) in buttons.iter() {
                btn.set_icon_name(if is_active(url) {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
            }
        }
        if let Some((row, url)) = self.ctx_episode_play.borrow().as_ref() {
            row.set_visible(!is_active(url));
        }
    }

    /// Streamt eine Podcast-Episode (ersetzt die laufende Wiedergabe). Startet
    /// an der gemerkten Position (Resume) und sichert vorher die Position einer
    /// bisher laufenden Episode.
    pub(crate) fn play_episode(&mut self, url: &str, title: &str) {
        let resume = self.library.episode_progress(url).unwrap_or(0);
        self.play_episode_from(url, title, resume);
    }

    /// Wie `play_episode`, startet aber an einer bestimmten Position (für die
    /// anklickbaren Sprungmarken in den Shownotes).
    pub(crate) fn play_episode_at(&mut self, url: &str, title: &str, ms: i64) {
        self.play_episode_from(url, title, ms.max(0));
    }

    /// Setzt die Kapitel der laufenden Wiedergabe: Seekbar-Marken **und** die
    /// geteilte Kapitelliste für die Hover-Anzeige. Leere Liste = löschen (z. B.
    /// bei Titeln ohne Kapitel). Die Marken positionieren sich automatisch neu,
    /// sobald die Dauer feststeht (der Tick aktualisiert den Wertebereich).
    pub(crate) fn set_chapters(&self, chapters: Vec<(i64, String)>) {
        self.seek_scale.clear_marks();
        for (ms, _) in &chapters {
            if *ms > 0 {
                self.seek_scale
                    .add_mark(*ms as f64, gtk::PositionType::Top, None);
            }
        }
        self.chapter_label.set_visible(false);
        *self.chapters.borrow_mut() = chapters;
    }

    fn play_episode_from(&mut self, url: &str, title: &str, resume: i64) {
        self.save_episode_progress();
        match self.player.play_uri(url, resume) {
            Ok(()) => {
                self.now_playing = Some(title.to_string());
                self.playing = true;
                self.playing_path = None;
                self.playing_episode_url = Some(url.to_string());
                self.queue.clear();
                self.queue_pos = 0;
                self.position_ms = resume.max(0);
                self.track_duration_ms = 0;
                *self.close_resume.borrow_mut() = None;
                self.mpris.set_metadata(0, title, None, None, None, None);
                self.mpris.set_playing(true);
                self.refresh_queue_icons();
                // Kapitel (Zeit + Bezeichnung) aus den Shownotes: Seekbar-Marken
                // setzen und für die Hover-Anzeige merken.
                let chapters = self
                    .library
                    .episode_description_by_url(url)
                    .ok()
                    .flatten()
                    .map(|d| crate::core::podcast::parse_chapters(&d))
                    .unwrap_or_default();
                self.set_chapters(chapters);
            }
            Err(e) => tracing::error!("Failed to play episode: {e}"),
        }
    }
}
