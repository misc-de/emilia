//! Dialoge: Aktionsmenü (langes Drücken), Teilen-Dialog und Einstellungen.
//! Aus app.rs herausgelöst – reine Umordnung, kein Funktionswechsel.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::ui::app::{App, CtxTarget, FsKind, Msg};

impl App {
    /// Aktionsmenü beim langen Drücken (Ordner oder Titel).
    pub(crate) fn open_context_menu(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(entry) = self.context_target.as_ref() else {
            return;
        };

        let dialog = adw::Dialog::builder().title(entry.heading()).build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Cover/Foto bzw. – bei mehreren Bildern – ein Karussell mit Punkten.
        self.append_cover_or_gallery(&content, entry, sender, &dialog);

        // "Mehr Infos" – aufklappbar mit Detailzeilen
        let info_group = adw::PreferencesGroup::new();
        let expander = adw::ExpanderRow::builder().title("Mehr Infos").build();
        for (label, value) in self.ctx_info_lines(entry) {
            let row = adw::ActionRow::builder()
                .title(label)
                .subtitle(gtk::glib::markup_escape_text(&value))
                .build();
            row.set_subtitle_lines(2);
            expander.add_row(&row);
        }
        info_group.add(&expander);
        content.append(&info_group);

        // "Merkmale" – Kategorie je Ebene (Titel/Album/Interpret), vererbt.
        if let Some(merkmale) = self.ctx_merkmale(entry, sender) {
            content.append(&merkmale);
        }

        // Aktionen
        let action_group = adw::PreferencesGroup::new();
        // Wiedergabe-Art des Ziels bestimmen (Label + Reihenfolge der Play-Aktion).
        #[derive(Clone, Copy)]
        enum PlayKind {
            Album,
            Artist,
            Other,
        }
        let play_kind = match entry {
            CtxTarget::Album(_) => PlayKind::Album,
            CtxTarget::Artist(_) => PlayKind::Artist,
            CtxTarget::Fs(e) if e.is_dir() => match self.fs_music_kind(e) {
                Some(FsKind::Album { .. }) => PlayKind::Album,
                Some(FsKind::Artist(_)) => PlayKind::Artist,
                None => PlayKind::Other,
            },
            CtxTarget::Fs(_) => PlayKind::Other,
        };
        // Equalizer dort anbieten, wo es eine eindeutige Ebene gibt: bei Titeln
        // und Karten sowie bei Ordnern, die als Interpret oder Album erkannt werden.
        let show_eq = !matches!(
            (entry, play_kind),
            (CtxTarget::Fs(e), PlayKind::Other) if e.is_dir()
        );

        // Play-Aktion: bei Album/Interpret eigener Text und eigene Reihenfolge.
        let play_row = adw::ActionRow::builder()
            .title(match play_kind {
                PlayKind::Album => "Album abspielen",
                PlayKind::Artist => "Interpreten abspielen",
                PlayKind::Other => "Abspielen",
            })
            .activatable(true)
            .build();
        play_row.add_prefix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
        match play_kind {
            PlayKind::Artist => {
                // Album-Reihenfolge wählbar, auf gleicher Höhe wie die Aktion.
                let order = gtk::DropDown::from_strings(&["Älteste zuerst", "Neueste zuerst"]);
                order.set_valign(gtk::Align::Center);
                order.set_tooltip_text(Some("Reihenfolge der Alben"));
                play_row.add_suffix(&order);
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlayArtist {
                        newest_first: order.selected() == 1,
                    });
                    dialog.close();
                });
            }
            PlayKind::Album => {
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlayAlbum);
                    dialog.close();
                });
            }
            PlayKind::Other => {
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlay);
                    dialog.close();
                });
            }
        }
        action_group.add(&play_row);

        // Übrige Aktionen.
        let mut actions: Vec<(&str, &str, fn() -> Msg)> = vec![
            ("Zur Queue hinzufügen", "list-add-symbolic", || Msg::CtxAddQueue),
            ("Zur Playlist hinzufügen", "view-list-symbolic", || {
                Msg::CtxAddPlaylist
            }),
        ];
        if show_eq {
            actions.push(("Equalizer-Einstellungen", "preferences-other-symbolic", || {
                Msg::CtxEqualizer
            }));
        }
        actions.push(("Teilen", "emblem-shared-symbolic", || Msg::CtxShare));
        for (label, icon, make_msg) in actions {
            let row = adw::ActionRow::builder()
                .title(label)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            let sender = sender.clone();
            let dialog = dialog.clone();
            row.connect_activated(move |_| {
                sender.input(make_msg());
                dialog.close();
            });
            action_group.add(&row);
        }
        content.append(&action_group);

        // Bei zu großem Inhalt (z. B. auf dem Phone) vertikal scrollen, sonst
        // den Dialog auf die natürliche Inhaltshöhe wachsen lassen. `Automatic`
        // blendet bei Überlauf einen Scrollbalken ein – mit `External` wurden die
        // unteren Aktionen (Equalizer, Teilen) auf schmalen Fenstern unerreichbar.
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// „Teilen"-Dialog: Verbindung anbieten (Dienst starten) oder QR-Code einlesen.
    /// Die eigentliche Geräte-Sync-Logik folgt später.
    pub(crate) fn open_share_dialog(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let dialog = adw::Dialog::builder().title("Teilen").build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let group = adw::PreferencesGroup::builder()
            .description("Mit einem anderen Gerät verbinden, um Inhalte zu synchronisieren.")
            .build();

        let actions: [(&str, &str, &str, fn() -> Msg); 2] = [
            (
                "Verbindung anbieten",
                "Dienst starten und auf ein anderes Gerät warten",
                "network-wireless-symbolic",
                || Msg::ShareHost,
            ),
            (
                "QR-Code einlesen",
                "Den Code eines anderen Geräts scannen",
                "camera-photo-symbolic",
                || Msg::ShareScan,
            ),
        ];

        for (title, subtitle, icon, make_msg) in actions {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            let sender = sender.clone();
            let dialog = dialog.clone();
            row.connect_activated(move |_| {
                sender.input(make_msg());
                dialog.close();
            });
            group.add(&row);
        }

        content.append(&group);

        // Bei zu großem Inhalt (z. B. auf dem Phone) vertikal scrollen, sonst
        // den Dialog auf die natürliche Inhaltshöhe wachsen lassen. `Automatic`
        // blendet bei Überlauf einen Scrollbalken ein.
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }
    /// Öffnet den Einstellungsdialog (u. a. Musikordner festlegen).
    pub(crate) fn open_settings(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::builder()
            .title("Einstellungen")
            .icon_name("emblem-system-symbolic")
            .build();
        let group = adw::PreferencesGroup::builder()
            .title("Bibliothek")
            .description("Startordner für die Dateisystem-Ansicht")
            .build();

        let current = self.music_dir.as_deref().unwrap_or("Nicht festgelegt");
        let row = adw::ActionRow::builder()
            .title("Musikordner")
            .subtitle(gtk::glib::markup_escape_text(current))
            .subtitle_lines(2)
            .build();

        let button = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Ordner wählen")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();

        {
            let sender = sender.clone();
            let win = root.clone();
            let row = row.clone();
            button.connect_clicked(move |_| {
                let chooser = gtk::FileDialog::builder()
                    .title("Musikordner wählen")
                    .build();
                let sender = sender.clone();
                let row = row.clone();
                chooser.select_folder(Some(&win), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(folder) = res {
                        if let Some(path) = folder.path() {
                            row.set_subtitle(&gtk::glib::markup_escape_text(
                                &path.to_string_lossy(),
                            ));
                            sender.input(Msg::SetMusicDir(path));
                        }
                    }
                });
            });
        }

        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));
        group.add(&row);
        page.add(&group);

        // Globaler Equalizer (Basis für alles ohne eigene Interpret-/Album-/Titel-EQ).
        let eq_group = adw::PreferencesGroup::builder()
            .title("Equalizer")
            .description(
                "Globale Klangregelung. Sie gilt überall, sofern nicht für einen \
                 Interpreten, ein Album oder einen Titel eine eigene Einstellung gesetzt ist.",
            )
            .build();
        let eq_row = adw::ActionRow::builder()
            .title("Globaler Equalizer")
            .subtitle("Zehn Bänder, je Ausgang")
            .activatable(true)
            .build();
        eq_row.add_prefix(&gtk::Image::from_icon_name("preferences-other-symbolic"));
        eq_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let sender = sender.clone();
            eq_row.connect_activated(move |_| sender.input(Msg::OpenGlobalEq));
        }
        eq_group.add(&eq_row);
        page.add(&eq_group);

        // Online-Erkennung: AcoustID-Key für die Titel-Erkennung per Fingerprint.
        let online_group = adw::PreferencesGroup::builder()
            .title("Online-Erkennung")
            .description(
                "Optionaler AcoustID-Key für die Titel-Erkennung per Fingerprint \
                 (kostenlos unter acoustid.org/new-application). Cover & Künstlerfotos \
                 funktionieren ohne Key.",
            )
            .build();
        let key_row = adw::EntryRow::builder().title("AcoustID API-Key").build();
        key_row.set_text(self.acoustid_key.as_deref().unwrap_or(""));
        key_row.set_show_apply_button(true);
        {
            let sender = sender.clone();
            key_row.connect_apply(move |r| {
                sender.input(Msg::SetAcoustidKey(r.text().to_string()));
            });
        }
        online_group.add(&key_row);

        let fanart_row = adw::EntryRow::builder()
            .title("fanart.tv API-Key (optional, für mehrere Interpreten-Fotos)")
            .build();
        fanart_row.set_text(self.fanart_key.as_deref().unwrap_or(""));
        fanart_row.set_show_apply_button(true);
        {
            let sender = sender.clone();
            fanart_row.connect_apply(move |r| {
                sender.input(Msg::SetFanartKey(r.text().to_string()));
            });
        }
        online_group.add(&fanart_row);

        let auto_row = adw::SwitchRow::builder()
            .title("Automatisch abrufen (nur WLAN)")
            .subtitle(
                "Beim Start fehlende Cover, Fotos & Titel im Hintergrund laden – \
                 nur bei nicht-getakteter Verbindung",
            )
            .active(self.auto_enrich)
            .build();
        {
            let sender = sender.clone();
            auto_row.connect_active_notify(move |r| {
                sender.input(Msg::SetAutoEnrich(r.is_active()));
            });
        }
        online_group.add(&auto_row);
        page.add(&online_group);

        // Bereiche: ausgeblendete Navigationspunkte wieder einblenden.
        let sections_group = adw::PreferencesGroup::builder().title("Bereiche").build();
        let concerts_row = adw::SwitchRow::builder()
            .title("Konzerte anzeigen")
            .subtitle("Menüpunkt „Konzerte“ in der Navigation")
            .active(!self.concerts_hidden)
            .build();
        {
            let sender = sender.clone();
            concerts_row.connect_active_notify(move |r| {
                sender.input(Msg::SetConcertsVisible(r.is_active()));
            });
        }
        sections_group.add(&concerts_row);
        page.add(&sections_group);

        dialog.add(&page);
        dialog.present(Some(root));
    }
}
