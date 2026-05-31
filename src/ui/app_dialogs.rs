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

        // "Eigenschaften" – Kategorie je Ebene (Titel/Album/Interpret), vererbt.
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
        // Beim gerade laufenden Titel keine „Abspielen"-Aktion anbieten.
        let is_current = if let CtxTarget::Fs(e) = entry {
            !e.is_dir()
                && self.now_playing.is_some()
                && self.queue.get(self.queue_pos).map(|p| p.as_path()) == Some(e.path().as_path())
        } else {
            false
        };

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
        if !is_current {
            action_group.add(&play_row);
        }

        // Favorit-Stern (Markieren/Entfernen).
        let is_fav = self.target_is_favorite(entry);
        let fav_row = adw::ActionRow::builder()
            .title(if is_fav {
                "Aus Favoriten entfernen"
            } else {
                "Zu Favoriten"
            })
            .activatable(true)
            .build();
        fav_row.add_prefix(&gtk::Image::from_icon_name("emilia-favorite-symbolic"));
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            fav_row.connect_activated(move |_| {
                sender.input(Msg::ToggleFavorite);
                dialog.close();
            });
        }
        action_group.add(&fav_row);

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
            .title("Bibliothek")
            .icon_name("folder-symbolic")
            .build();
        let group = adw::PreferencesGroup::builder()
            .title("Startordner")
            .description("Ordner für die Dateisystem-Ansicht")
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
        dialog.add(&page);

        // --- Kategorie: Klang ---
        let page = adw::PreferencesPage::builder()
            .title("Klang")
            .icon_name("preferences-other-symbolic")
            .build();
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
        dialog.add(&page);

        // --- Kategorie: Online ---
        let page = adw::PreferencesPage::builder()
            .title("Online")
            .icon_name("network-wireless-symbolic")
            .build();
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
        dialog.add(&page);

        // --- Kategorie: Ansicht ---
        let page = adw::PreferencesPage::builder()
            .title("Ansicht")
            .icon_name("view-list-symbolic")
            .build();
        // Menüpunkte ein-/ausblenden **und** per Ziehgriff umsortieren. Die
        // Reihenfolge/Sichtbarkeit wird sofort in die Navigation übernommen.
        let sections_group = adw::PreferencesGroup::builder()
            .title("Menüpunkte")
            .description(
                "Ziehgriff zum Umsortieren; der Schalter blendet einen Menüpunkt aus. Beides wirkt sofort in der Navigation und der Eigenschaften-Auswahl.",
            )
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        // Gemeinsamer, lokaler Zustand des Dialogs (parallel zum Modell).
        let order = std::rc::Rc::new(std::cell::RefCell::new(self.section_order.clone()));
        let hidden = std::rc::Rc::new(std::cell::RefCell::new(self.hidden_sections.clone()));
        rebuild_section_rows(&list, &order, &hidden, sender);
        sections_group.add(&list);
        page.add(&sections_group);

        dialog.add(&page);
        dialog.present(Some(root));
    }

    /// Dateidialog zum Hochladen eines eigenen Covers/Fotos für das aktuelle
    /// Detailziel (Album → Cover, Interpret → Foto). Das gewählte Bild wird in
    /// den Cache kopiert und als primäres Bild gesetzt.
    pub(crate) fn open_cover_upload_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        enum Dest {
            Album(String, String),
            Artist(String),
        }
        let dest = match self.context_target.as_ref() {
            Some(CtxTarget::Album(m)) => Some(Dest::Album(m.artist.clone(), m.album.clone())),
            Some(CtxTarget::Artist(m)) => Some(Dest::Artist(m.name.clone())),
            // Ordner im Dateibrowser: als Album bzw. Interpret auflösen.
            _ => match self.ctx_album() {
                Some((a, al)) => Some(Dest::Album(a, al)),
                None => self.ctx_artist().map(Dest::Artist),
            },
        };
        let Some(dest) = dest else {
            self.toast("Hier lässt sich kein eigenes Bild setzen");
            return;
        };

        let filter = gtk::FileFilter::new();
        filter.add_pixbuf_formats();
        filter.set_name(Some("Bilder"));
        let chooser = gtk::FileDialog::builder()
            .title("Eigenes Bild auswählen")
            .default_filter(&filter)
            .build();

        let sender = sender.clone();
        chooser.open(Some(root), gtk::gio::Cancellable::NONE, move |res| {
            let Ok(file) = res else {
                return;
            };
            let Some(src) = file.path() else {
                return;
            };
            let is_artist = matches!(dest, Dest::Artist(_));
            let Some(cached) = store_custom_image(&src, is_artist) else {
                return;
            };
            match dest {
                Dest::Album(artist, album) => sender.input(Msg::SetAlbumCover {
                    artist,
                    album,
                    path: cached,
                }),
                Dest::Artist(name) => sender.input(Msg::SetArtistImage { name, path: cached }),
            }
        });
    }
}

/// Kopiert ein gewähltes Bild in den Cover- bzw. Künstler-Cache und gibt den
/// neuen Pfad zurück. Der Dateiname ist eindeutig (Zeitstempel), damit das Bild
/// sofort frisch geladen wird und kein alter Cache-Eintrag greift.
fn store_custom_image(src: &std::path::Path, is_artist: bool) -> Option<String> {
    let dir = if is_artist {
        crate::core::online::artist_cache_dir()
    } else {
        crate::core::online::cover_cache_dir()
    };
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 5)
        .unwrap_or("img");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out = dir.join(format!("custom_{stamp}.{ext}"));
    std::fs::copy(src, &out).ok()?;
    Some(out.to_string_lossy().into_owned())
}

/// Baut die Menüpunkt-Zeilen (Ziehgriff, Beschriftung, Sichtbarkeits-Schalter)
/// in der aktuellen Reihenfolge neu auf. Per Ziehen umsortierbar; jede Änderung
/// aktualisiert den lokalen Dialog-Zustand (`order`/`hidden`) und meldet sie dem
/// Modell, das Navigation und Reihenfolge sofort übernimmt.
fn rebuild_section_rows(
    list: &gtk::ListBox,
    order: &std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    hidden: &std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    sender: &ComponentSender<App>,
) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
    let names: Vec<&'static str> = order.borrow().clone();
    for (idx, &name) in names.iter().enumerate() {
        let Some((label, _icon)) = crate::ui::app::section_meta(name) else {
            continue;
        };
        let row = adw::ActionRow::builder().title(label).build();

        // Ziehgriff links (Hinweis); gezogen wird die ganze Zeile.
        let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
        handle.set_tooltip_text(Some("Zum Umsortieren ziehen"));
        row.add_prefix(&handle);

        let drag = gtk::DragSource::new();
        drag.set_actions(gtk::gdk::DragAction::MOVE);
        {
            let name = name.to_string();
            drag.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&name.to_value()))
            });
        }
        row.add_controller(drag);

        // DropTarget auf der ganzen Zeile: Quelle an diese Position verschieben.
        let drop = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
        {
            let (list, order, hidden, sender) =
                (list.clone(), order.clone(), hidden.clone(), sender.clone());
            drop.connect_drop(move |_, value, _, _| {
                let Ok(src) = value.get::<String>() else {
                    return false;
                };
                let to = idx;
                let from = order.borrow().iter().position(|n| *n == src.as_str());
                let (Some(from), Some(name_static)) = (
                    from,
                    crate::ui::app::SECTIONS
                        .iter()
                        .map(|(n, _, _)| *n)
                        .find(|n| *n == src.as_str()),
                ) else {
                    return false;
                };
                if from == to {
                    return false;
                }
                {
                    let mut o = order.borrow_mut();
                    o.remove(from);
                    o.insert(to, name_static);
                }
                sender.input(Msg::MoveSection { from, to });
                rebuild_section_rows(&list, &order, &hidden, &sender);
                true
            });
        }
        row.add_controller(drop);

        // Sichtbarkeits-Schalter rechts.
        let sw = gtk::Switch::builder()
            .active(!hidden.borrow().contains(name))
            .valign(gtk::Align::Center)
            .build();
        {
            let (hidden, sender) = (hidden.clone(), sender.clone());
            sw.connect_active_notify(move |s| {
                if s.is_active() {
                    hidden.borrow_mut().remove(name);
                } else {
                    hidden.borrow_mut().insert(name.to_string());
                }
                sender.input(Msg::SetSectionVisible {
                    section: name,
                    visible: s.is_active(),
                });
            });
        }
        row.add_suffix(&sw);

        list.append(&row);
    }
}
