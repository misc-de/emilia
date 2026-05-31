//! Ansichten und Daten-Helfer: Ordner/Album/Interpret laden und gruppieren, die
//! Unterseiten (Interpret → Alben → Titel) bauen, sowie die Kontext-/Detail-
//! Helfer (ctx_*) und die Cover-Auflösung. Aus app.rs herausgelöst – reine
//! Umordnung, kein Funktionswechsel.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category::{self, Category};
use crate::core::db::Library;
use crate::core::{cover, scanner};
use crate::model::{ArtistMeta, Track};
use crate::ui::app::{
    album_subtitle, cover_widget, duration_label, find_scroller, fmt_duration, most_common_artist,
    read_entries, App, Cmd, CtxTarget, FsKind, Msg,
};
use crate::ui::enrich::enrich_worker;
use crate::ui::fs_row::FsEntry;

impl App {
    /// Scroller der Dateiliste (Vorfahre der Einträge-`ListBox`).
    pub(crate) fn fs_scroller(&self) -> Option<gtk::ScrolledWindow> {
        self.entries
            .widget()
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
    }

    /// Startet das Einlesen des aktuellen Ordners im Hintergrund (mit Spinner).
    pub(crate) fn load_dir(&mut self, sender: &ComponentSender<Self>) {
        // Scrollposition des gerade gezeigten Ordners merken, bevor er ersetzt wird.
        if let (Some(dir), Some(sc)) = (self.shown_dir.clone(), self.fs_scroller()) {
            self.fs_scroll
                .borrow_mut()
                .insert(dir, sc.vadjustment().value());
        }
        match self.browse_dir.clone() {
            Some(dir) => {
                // Aktuellen Ordner merken (für „weitermachen, wo man war").
                let _ = self.library.set_setting("browse_dir", &dir.to_string_lossy());
                self.loading = true;
                sender.spawn_oneshot_command(move || Cmd::Entries(read_entries(dir)));
            }
            None => {
                self.entries.guard().clear();
                self.loading = false;
            }
        }
    }

    /// Lädt die Album-Übersicht aus der DB in die Factory (inkl. Online-Cover).
    pub(crate) fn reload_albums(&mut self) {
        let albums = self.library.albums_overview().unwrap_or_default();
        self.album_count = albums.len();
        let mut guard = self.albums.guard();
        guard.clear();
        for a in albums {
            guard.push_back(a);
        }
    }

    /// Liest die Bibliothek (Tags → DB) **im Hintergrund** ein – rein lokal, ohne
    /// Netz. `then_enrich`: danach ggf. automatisch online nachladen (entscheidet
    /// der `ScanDone`-Handler anhand Schalter + Verbindung).
    pub(crate) fn start_scan(&self, sender: &ComponentSender<Self>, then_enrich: bool) {
        let Some(root) = self.root_dir.clone() else {
            return;
        };
        sender.spawn_oneshot_command(move || {
            match Library::open() {
                Ok(lib) => {
                    if let Err(e) = scanner::scan_into(&lib, &root) {
                        tracing::warn!("Bibliotheks-Scan fehlgeschlagen: {e}");
                    }
                }
                Err(e) => tracing::error!("DB für Scan nicht erreichbar: {e}"),
            }
            Cmd::ScanDone { then_enrich }
        });
    }

    /// Startet die Online-Anreicherung im Hintergrund. `scan_first`: zuvor noch die
    /// Tags einlesen (beim manuellen Abruf) – beim automatischen Lauf entfällt das,
    /// weil der lokale Scan bereits durchlief. Die Audiodateien werden dabei nur
    /// gelesen, niemals verändert.
    pub(crate) fn run_enrich(
        &mut self,
        sender: &ComponentSender<Self>,
        scan_first: bool,
        auto: bool,
    ) {
        let Some(root) = self.root_dir.clone() else {
            self.toast("Kein Musikordner festgelegt – bitte in den Einstellungen wählen");
            return;
        };
        if self.enriching {
            return;
        }
        // Fehlender AcoustID-Key/fpcalc: Titel-Erkennung wird still übersprungen.
        let key = self.acoustid_key.clone();
        let fkey = self.fanart_key.clone();
        self.enrich_cancel.store(false, Ordering::Relaxed);
        let cancel = self.enrich_cancel.clone();
        self.enriching = true;
        // Neuer Lauf → Fortschritts-Leiste wieder einblenden.
        self.enrich_banner_hidden = false;
        self.enrich_status = if scan_first {
            "Bibliothek wird eingelesen …".to_string()
        } else {
            "Cover & Metadaten werden gesucht …".to_string()
        };
        sender
            .spawn_command(move |out| enrich_worker(root, key, fkey, cancel, scan_first, auto, &out));
    }

    /// Lädt die Interpreten-Übersicht aus der DB in die Factory (inkl. Foto).
    pub(crate) fn reload_artists(&mut self) {
        let artists = self.library.artists_overview().unwrap_or_default();
        self.artist_count = artists.len();
        let mut guard = self.artists.guard();
        guard.clear();
        for a in artists {
            guard.push_back(a);
        }
    }

    /// Liefert die abspielbaren Dateien eines Eintrags: bei Ordnern rekursiv,
    /// bei Dateien nur die eine.
    pub(crate) fn entry_files(&self, entry: &FsEntry) -> Vec<PathBuf> {
        if entry.is_dir() {
            scanner::collect_audio_files(entry.path())
        } else {
            vec![entry.path().clone()]
        }
    }

    /// Alle Dateien eines Interpreten (aus der Bibliothek), in Abspielreihenfolge.
    pub(crate) fn artist_files(&self, name: &str) -> Vec<PathBuf> {
        // Wie die Interpreten-Liste (artist_sections/artist_albums): ein Titel
        // zählt zum Interpreten, wenn dessen Name in der – ggf. aus „feat."
        // zerlegten – Interpreten-Angabe vorkommt (case-insensitiv). Sonst zählte
        // die Detailseite Gast-/zusammengesetzte Titel nicht mit und zeigte „0
        // Lieder", obwohl die Liederliste sie führt.
        let target = crate::core::artist::norm_key(name);
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.artist.as_deref().is_some_and(|a| {
                    crate::core::artist::split_artists(a)
                        .iter()
                        .any(|s| crate::core::artist::norm_key(s) == target)
                })
            })
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// Alle Dateien eines Albums (Haupt-Interpret + Album), in Abspielreihenfolge.
    /// Zählt feat.-Varianten desselben Haupt-Interpreten mit – passend zur
    /// zusammengefassten Alben-Übersicht.
    pub(crate) fn album_files(&self, artist: &str, album: &str) -> Vec<PathBuf> {
        let target = crate::core::artist::norm_key(artist);
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.album.as_deref() == Some(album)
                    && t.artist.as_deref().is_some_and(|a| {
                        crate::core::artist::split_artists(a)
                            .first()
                            .is_some_and(|p| crate::core::artist::norm_key(p) == target)
                    })
            })
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// Alle Titel eines (ggf. aus „feat." zerlegten) Interpreten, nach Album
    /// gruppiert. Ein Titel zählt zum Interpreten, wenn dessen Name in der
    /// zerlegten Interpreten-Angabe des Titels vorkommt (case-insensitiv) –
    /// passend zur Interpreten-Liste, die ebenfalls „feat."-Angaben aufteilt.
    /// Alben in der Reihenfolge aus `all_tracks` (alphabetisch), Titel je Album
    /// nach Tracknummer.
    pub(crate) fn artist_albums(&self, name: &str) -> Vec<(String, Vec<Track>)> {
        let target = crate::core::artist::norm_key(name);
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for t in self.library.all_tracks().unwrap_or_default() {
            let belongs = t.artist.as_deref().is_some_and(|a| {
                crate::core::artist::split_artists(a)
                    .iter()
                    .any(|s| crate::core::artist::norm_key(s) == target)
            });
            if !belongs {
                continue;
            }
            let album = t.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(t);
        }
        order
            .into_iter()
            .map(|album| {
                let tracks = groups.remove(&album).unwrap_or_default();
                (album, tracks)
            })
            .collect()
    }

    /// Teilt die Titel eines Interpreten in **eigene Alben** und **Einzellieder**:
    ///
    /// * Gehören dem Interpreten **alle** Titel eines Albums (laut Bibliothek), ist
    ///   es sein Album → eigener Album-Eintrag `(Album, Anzeige-Interpret, Titel)`.
    /// * Ist er nur auf **einem Teil** des Albums vertreten (z. B. als Gast auf
    ///   2–3 Stücken), zählen diese Titel zu den Einzelliedern.
    /// * Titel ganz ohne Album sind ebenfalls Einzellieder.
    ///
    /// Alben in der Reihenfolge aus `all_tracks`; Titel je Album nach Tracknummer.
    pub(crate) fn artist_sections(&self, name: &str) -> (Vec<(String, String, Vec<Track>)>, Vec<Track>) {
        let target = crate::core::artist::norm_key(name);
        let all = self.library.all_tracks().unwrap_or_default();

        // Titel des Interpreten nach Albumname gruppieren (Reihenfolge bewahren).
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for t in all {
            let belongs = t.artist.as_deref().is_some_and(|a| {
                crate::core::artist::split_artists(a)
                    .iter()
                    .any(|s| crate::core::artist::norm_key(s) == target)
            });
            if !belongs {
                continue;
            }
            let album = t.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(t);
        }

        let mut albums: Vec<(String, String, Vec<Track>)> = Vec::new();
        let mut singles: Vec<Track> = Vec::new();
        for album in order {
            let mine = groups.remove(&album).unwrap_or_default();
            if album.is_empty() {
                singles.extend(mine);
                continue;
            }
            // „Eigenes Album": der Interpret ist auf der Mehrheit der ihm
            // zugeordneten Titel der erstgenannte (Haupt-)Interpret. Ist er
            // überall nur Gast (… feat. <name>), zählen die Titel als Einzellieder.
            let own = mine
                .iter()
                .filter(|t| {
                    t.artist.as_deref().is_some_and(|a| {
                        crate::core::artist::split_artists(a)
                            .first()
                            .is_some_and(|p| crate::core::artist::norm_key(p) == target)
                    })
                })
                .count();
            // Nur als Album zeigen, wenn mindestens zwei Titel vorhanden sind –
            // ein einzelnes Lied (z. B. nur ein Stück eines Albums in der
            // Bibliothek) zählt als Einzellied, nicht als Album.
            if mine.len() >= 2 && own > 0 && own * 2 >= mine.len() {
                let display_artist = most_common_artist(&mine);
                albums.push((album, display_artist, mine));
            } else {
                singles.extend(mine);
            }
        }
        (albums, singles)
    }

    /// Titel, die zu „diesem Album dieses Interpreten" gehören: alle Bibliotheks-
    /// titel mit dem Albumnamen, in deren (zerlegter) Interpreten-Angabe `name`
    /// vorkommt. Bereits nach Tracknummer sortiert (Reihenfolge aus `all_tracks`).
    pub(crate) fn album_tracks_for_artist(&self, name: &str, album: &str) -> Vec<Track> {
        let target = crate::core::artist::norm_key(name);
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                // Album-Zugehörigkeit über den Haupt-Interpreten (wie die
                // Alben-Übersicht): „A feat. B" gehört zum Album von „A".
                t.album.as_deref() == Some(album)
                    && t.artist.as_deref().is_some_and(|a| {
                        crate::core::artist::split_artists(a)
                            .first()
                            .is_some_and(|p| crate::core::artist::norm_key(p) == target)
                    })
            })
            .collect()
    }

    /// Hüllt einen Inhalt in eine scrollbare Unterseite (mit Kopfleiste +
    /// Zurück-Pfeil) und schiebt sie auf den Navigations-Stapel.
    pub(crate) fn push_subpage(&self, title: &str, content: &gtk::Box) {
        // Verlassen wir die Wurzel-Übersicht, die aktuelle Scrollposition der
        // sichtbaren Sektion merken (wird beim Zurückkehren wiederhergestellt).
        let leaving_root = self
            .nav_view
            .visible_page()
            .and_then(|p| p.tag())
            .is_some_and(|t| t == "main");
        if leaving_root {
            if let Some(sc) = self
                .view_stack
                .visible_child()
                .and_then(|c| find_scroller(&c))
            {
                let value = sc.vadjustment().value();
                *self.overview_scroll.borrow_mut() = Some((sc, value));
            }
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        let page = adw::NavigationPage::builder()
            .title(title)
            .child(&toolbar)
            .build();
        self.nav_view.push(&page);
    }

    /// Kurzes Tippen auf einen Interpreten: öffnet eine Unterseite, die zuerst
    /// dessen **Alben** (mit Cover) und danach die **Einzellieder** (Titel ohne
    /// Album, mit Cover) auflistet. Tippen auf ein Album öffnet dessen Titel als
    /// weitere Unterseite; Tippen auf ein Einzellied spielt es ab.
    pub(crate) fn open_artist_tracks(&self, sender: &ComponentSender<Self>, meta: &ArtistMeta) {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Eigene Alben vom Rest (Gast-Titel + Titel ohne Album) trennen.
        let (album_groups, singles) = self.artist_sections(&meta.name);

        if album_groups.is_empty() && singles.is_empty() {
            content.append(
                &adw::StatusPage::builder()
                    .icon_name("avatar-default-symbolic")
                    .title("Keine Titel")
                    .description("Für diesen Interpreten sind keine Lieder in der Bibliothek.")
                    .build(),
            );
        }

        // --- Alben zuerst ---
        if !album_groups.is_empty() {
            let n = album_groups.len();
            let group = adw::PreferencesGroup::builder()
                .title("Alben")
                .description(format!("{n} {}", if n == 1 { "Album" } else { "Alben" }))
                .build();
            for (album, display_artist, tracks) in &album_groups {
                let album_meta = self
                    .library
                    .get_album_meta(display_artist, album)
                    .ok()
                    .flatten();
                let year = album_meta.as_ref().and_then(|m| m.year);
                let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());

                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(album))
                    .subtitle(album_subtitle(year, tracks.len()))
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                row.add_prefix(&cover_widget(cover_path.as_deref(), "media-optical-symbolic"));

                // Gesamtlaufzeit aller Albumtitel + Play-Button (Layout wie bei
                // den Einzelliedern). Der Button spielt das ganze Album ab; ein
                // Tippen auf die Zeile öffnet weiterhin die Album-Unterseite.
                let total_ms: i64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
                if total_ms > 0 {
                    row.add_suffix(&duration_label(total_ms));
                }
                let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
                play.add_css_class("flat");
                play.set_valign(gtk::Align::Center);
                play.set_tooltip_text(Some("Album abspielen"));
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let album = album.clone();
                    play.connect_clicked(move |_| {
                        sender.input(Msg::PlayAlbum {
                            artist: name.clone(),
                            album: album.clone(),
                        });
                    });
                }
                row.add_suffix(&play);

                let album = album.clone();
                let display_artist = display_artist.clone();
                // Kurzes Tippen: Album-Unterseite (Lieder des Albums).
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let album = album.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::OpenAlbumTracks {
                            artist: name.clone(),
                            album: album.clone(),
                        });
                    });
                }
                // Langes Drücken: Album-Detailansicht.
                {
                    let sender = sender.clone();
                    let gesture = gtk::GestureLongPress::new();
                    gesture.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::ShowAlbumDetailFor {
                            artist: display_artist.clone(),
                            album: album.clone(),
                        });
                    });
                    row.add_controller(gesture);
                }
                group.add(&row);
            }
            content.append(&group);
        }

        // --- danach die Einzellieder (Gast-Titel + Titel ohne Album) ---
        if !singles.is_empty() {
            let n = singles.len();
            let group = adw::PreferencesGroup::builder()
                .title("Einzellieder")
                .description(format!("{n} {}", if n == 1 { "Lied" } else { "Lieder" }))
                .build();
            for t in &singles {
                // Cover-Reihenfolge (nie ein fremdes Ordnerbild):
                // 1) eingebettetes Bild des Titels selbst,
                // 2) Cover des tatsächlichen Albums (auch bei Gast-Titeln),
                // 3) Foto des Haupt-Interpreten.
                let cover_path = crate::core::online::local_track_cover(&t.path)
                    .or_else(|| {
                        let album = t.album.as_deref().filter(|a| !a.trim().is_empty())?;
                        let artist = t.artist.as_deref().unwrap_or("");
                        // Erst exakt (Interpret, Album), sonst irgendein Cover des Albums.
                        self.library
                            .get_album_meta(artist, album)
                            .ok()
                            .flatten()
                            .and_then(|m| m.cover_path)
                            .or_else(|| self.library.album_cover(album).ok().flatten())
                    })
                    .or_else(|| {
                        let artist = t.artist.as_deref().filter(|a| !a.trim().is_empty())?;
                        let primary =
                            crate::core::artist::split_artists(artist).into_iter().next()?;
                        self.library
                            .get_artist_meta(&primary)
                            .ok()
                            .flatten()
                            .and_then(|m| m.image_path)
                    });
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&t.title))
                    .activatable(true)
                    .build();
                // Album als Sekundär-Info unter dem Liednamen (falls vorhanden).
                if let Some(al) = t.album.as_deref().filter(|a| !a.trim().is_empty()) {
                    row.set_subtitle(&gtk::glib::markup_escape_text(al));
                }
                row.add_css_class("emilia-flush");
                row.add_prefix(&cover_widget(cover_path.as_deref(), "audio-x-generic-symbolic"));
                if let Some(ms) = t.duration_ms {
                    if ms > 0 {
                        row.add_suffix(&duration_label(ms));
                    }
                }
                row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

                let path = t.path.clone();
                // Kurzes Tippen: Titel abspielen.
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let path = path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::PlayArtistTrack {
                            name: name.clone(),
                            path: path.clone(),
                        });
                    });
                }
                // Langes Drücken: Detailansicht des Liedes.
                {
                    let sender = sender.clone();
                    let gesture = gtk::GestureLongPress::new();
                    gesture.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::ShowTrackDetail(path.clone()));
                    });
                    row.add_controller(gesture);
                }
                group.add(&row);
            }
            content.append(&group);
        }

        self.push_subpage(&meta.name, &content);
    }

    /// Tippen auf ein Album in der Interpreten-Unterseite: listet dessen Titel
    /// (mit Album-Cover) als weitere Unterseite auf. Tippen auf einen Titel
    /// spielt das gesamte Album ab diesem Titel ab.
    pub(crate) fn open_album_tracks(&self, sender: &ComponentSender<Self>, name: &str, album: &str) {
        // Titel des Albums – `all_tracks` liefert bereits nach Tracknummer sortiert.
        let tracks = self.album_tracks_for_artist(name, album);

        // Cover/Jahr liegen unter dem (häufigsten) rohen Interpreten-Credit.
        let display_artist = most_common_artist(&tracks);
        let album_meta = self
            .library
            .get_album_meta(&display_artist, album)
            .ok()
            .flatten();
        let year = album_meta.as_ref().and_then(|m| m.year);
        let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());
        // Album-Cover einmal dekodieren und in allen Titelzeilen wiederverwenden.
        let cover = cover_path
            .as_deref()
            .and_then(crate::ui::widgets::thumb_cached);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Vorhandene Discs ermitteln (None gilt als CD 1). Mehr als eine → die
        // Titel werden nach „CD 1" / „CD 2" … getrennt dargestellt.
        let mut discs: Vec<u32> = tracks.iter().map(|t| t.disc_no.unwrap_or(1)).collect();
        discs.sort_unstable();
        discs.dedup();
        let multi_disc = discs.len() > 1;

        // Baut eine Titelzeile (Cover, Tracknummer, Dauer, Play + Gesten).
        let make_row = |t: &Track| -> adw::ActionRow {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&t.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            row.add_prefix(&crate::ui::widgets::rounded_image(
                cover.as_ref(),
                "media-optical-symbolic",
                48,
            ));
            if let Some(no) = t.track_no {
                row.add_prefix(
                    &gtk::Label::builder()
                        .label(no.to_string())
                        .width_chars(2)
                        .xalign(1.0)
                        .css_classes(["dim-label", "numeric"])
                        .build(),
                );
            }
            if let Some(ms) = t.duration_ms {
                if ms > 0 {
                    row.add_suffix(&duration_label(ms));
                }
            }
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

            let path = t.path.clone();
            // Kurzes Tippen: Titel abspielen (ganzes Album ab hier).
            {
                let sender = sender.clone();
                let name = name.to_string();
                let album = album.to_string();
                let path = path.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::PlayAlbumTrack {
                        artist: name.clone(),
                        album: album.clone(),
                        path: path.clone(),
                    });
                });
            }
            // Langes Drücken: Detailansicht des Liedes.
            {
                let sender = sender.clone();
                let gesture = gtk::GestureLongPress::new();
                gesture.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowTrackDetail(path.clone()));
                });
                row.add_controller(gesture);
            }
            row
        };

        if multi_disc {
            for (i, disc) in discs.iter().enumerate() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("CD {disc}"))
                    .build();
                // Album-Jahr/Titelzahl als Untertitel der ersten Disc-Gruppe.
                if i == 0 {
                    group.set_description(Some(
                        album_subtitle(year, tracks.len()).as_str(),
                    ));
                }
                for t in tracks.iter().filter(|t| t.disc_no.unwrap_or(1) == *disc) {
                    group.add(&make_row(t));
                }
                content.append(&group);
            }
        } else {
            let group = adw::PreferencesGroup::builder()
                .title(gtk::glib::markup_escape_text(album))
                .description(album_subtitle(year, tracks.len()))
                .build();
            for t in &tracks {
                group.add(&make_row(t));
            }
            content.append(&group);
        }

        // Kopfzeile: bevorzugt der Album-Interpret, sonst der Seiten-Interpret.
        let header_artist = if display_artist.is_empty() {
            name
        } else {
            display_artist.as_str()
        };
        let title = if header_artist.is_empty() {
            album.to_string()
        } else {
            format!("{header_artist} – {album}")
        };
        self.push_subpage(&title, &content);
    }

    // ---- Ziel-abhängige Helfer für die Detailansicht (Datei/Ordner, Interpret, Album) ----

    /// Abspielbare Dateien des Detailziels.
    pub(crate) fn ctx_files(&self, target: &CtxTarget) -> Vec<PathBuf> {
        match target {
            CtxTarget::Fs(e) => self.entry_files(e),
            CtxTarget::Artist(m) => self.artist_files(&m.name),
            CtxTarget::Album(m) => self.album_files(&m.artist, &m.album),
        }
    }

    /// Cover-/Foto-Textur plus passendes Platzhalter-Icon.
    /// Erkennt, ob ein Dateisystem-Ordner einem Interpreten oder einem Album
    /// entspricht, und liefert die passende EQ-Ebene als
    /// `(Überschrift, Hinweis, scope, key)` – passend zu [`Self::open_eq_editor`].
    /// So lässt sich der Equalizer direkt aus der Dateiansicht auf Interpret- bzw.
    /// Album-Ebene einstellen, mit denselben Schlüsseln wie in der Interpreten-/
    /// Album-Übersicht (damit sich die Einstellungen nicht doppeln).
    /// Erkennt, ob ein Dateisystem-Ordner einem Interpreten oder einem Album
    /// entspricht. Grundlage für Wiedergabe („Album/Interpreten abspielen") und
    /// die EQ-Ebene aus der Dateiansicht.
    pub(crate) fn fs_music_kind(&self, entry: &FsEntry) -> Option<FsKind> {
        if !entry.is_dir() {
            return None;
        }
        // Ordnername = bekannter Interpret? → Interpret (gleicher Schlüssel wie
        // in der Interpreten-Übersicht).
        if let Ok(Some(meta)) = self.library.get_artist_meta(entry.name()) {
            return Some(FsKind::Artist(meta.name));
        }
        // Sonst: enthält der Ordner Titel genau eines Albums? → Album.
        let dir = entry.path();
        let tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| std::path::Path::new(&t.path).starts_with(dir))
            .collect();
        let albums: std::collections::HashSet<&str> = tracks
            .iter()
            .filter_map(|t| t.album.as_deref())
            .filter(|a| !a.is_empty())
            .collect();
        if albums.len() == 1 {
            let album = albums.into_iter().next().unwrap().to_string();
            let artist = tracks
                .iter()
                .find_map(|t| t.artist.clone())
                .unwrap_or_default();
            return Some(FsKind::Album { artist, album });
        }
        None
    }

    /// EQ-Ebene `(Überschrift, Hinweis, scope, key)` eines Dateisystem-Ordners,
    /// passend zu [`Self::open_eq_editor`] – leitet sich aus [`Self::fs_music_kind`] ab.
    pub(crate) fn fs_eq_level(
        &self,
        entry: &FsEntry,
    ) -> Option<(&'static str, String, Option<&'static str>, &'static str, String)> {
        match self.fs_music_kind(entry)? {
            FsKind::Artist(name) => Some((
                "den Interpreten",
                name.clone(),
                Some("Gilt auch für die Alben und Lieder dieses Interpreten."),
                "artist",
                name,
            )),
            FsKind::Album { artist, album } => {
                let key = category::album_key(&artist, &album);
                Some((
                    "das Album",
                    album,
                    Some("Gilt auch für die Lieder dieses Albums."),
                    "album",
                    key,
                ))
            }
        }
    }

    /// Album-Identität (Interpret, Album) des aktuellen Kontextziels, falls es ein
    /// Album ist (Album-Karte oder als Album erkannter Ordner).
    pub(crate) fn ctx_album(&self) -> Option<(String, String)> {
        match self.context_target.as_ref()? {
            CtxTarget::Album(m) => Some((m.artist.clone(), m.album.clone())),
            CtxTarget::Fs(e) => match self.fs_music_kind(e)? {
                FsKind::Album { artist, album } => Some((artist, album)),
                FsKind::Artist(_) => None,
            },
            CtxTarget::Artist(_) => None,
        }
    }

    /// Interpretenname des aktuellen Kontextziels, falls es ein Interpret ist
    /// (Interpreten-Karte oder als Interpret erkannter Ordner).
    pub(crate) fn ctx_artist(&self) -> Option<String> {
        match self.context_target.as_ref()? {
            CtxTarget::Artist(m) => Some(m.name.clone()),
            CtxTarget::Fs(e) => match self.fs_music_kind(e)? {
                FsKind::Artist(name) => Some(name),
                FsKind::Album { .. } => None,
            },
            CtxTarget::Album(_) => None,
        }
    }

    /// Alben eines Interpreten mit (sofern bekannt) Erscheinungsjahr aus den
    /// Album-Metadaten. Titel je Album bereits nach Tracknummer (siehe
    /// [`Self::artist_albums`]).
    pub(crate) fn artist_albums_dated(&self, name: &str) -> Vec<(Option<i32>, String, Vec<Track>)> {
        self.artist_albums(name)
            .into_iter()
            .map(|(album, tracks)| {
                let artist = tracks
                    .first()
                    .and_then(|t| t.artist.clone())
                    .unwrap_or_default();
                let year = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .and_then(|m| m.year);
                (year, album, tracks)
            })
            .collect()
    }

    /// Alle Titel eines Interpreten in Abspielreihenfolge: Alben nach Jahr
    /// (älteste bzw. neueste zuerst, unbekannte Jahre ans Ende), je Album von
    /// Track 1 top-down.
    pub(crate) fn artist_files_ordered(&self, name: &str, newest_first: bool) -> Vec<PathBuf> {
        let mut albums = self.artist_albums_dated(name);
        albums.sort_by(|a, b| {
            use std::cmp::Ordering;
            let by_year = match (a.0, b.0) {
                (Some(x), Some(y)) => {
                    if newest_first {
                        y.cmp(&x)
                    } else {
                        x.cmp(&y)
                    }
                }
                // Bekanntes Jahr vor unbekanntem (in beiden Richtungen).
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            by_year.then_with(|| a.1.cmp(&b.1))
        });
        albums
            .into_iter()
            .flat_map(|(_, _, tracks)| tracks.into_iter().map(|t| PathBuf::from(t.path)))
            .collect()
    }

    /// Jahres-Info der Alben eines Interpreten als `(Label, Wert)`: bei mindestens
    /// zwei **unterschiedlichen** Jahren „Jahre" + „von – bis", bei genau einem
    /// bekannten Jahr „Jahr" + Einzeljahr. `None`, wenn kein Jahr bekannt ist.
    pub(crate) fn artist_year_range(&self, name: &str) -> Option<(&'static str, String)> {
        let mut years: Vec<i32> = self
            .artist_albums_dated(name)
            .into_iter()
            .filter_map(|(year, _, _)| year)
            .collect();
        years.sort_unstable();
        years.dedup();
        match years.as_slice() {
            [] => None,
            [y] => Some(("Jahr", y.to_string())),
            _ => Some(("Jahre", format!("{} – {}", years[0], years[years.len() - 1]))),
        }
    }

    pub(crate) fn ctx_cover(&self, target: &CtxTarget) -> (Option<gtk::gdk::Texture>, &'static str) {
        match target {
            CtxTarget::Fs(e) => {
                // Zuerst ein (Album-)Cover: Cover-Datei, eingebettet, oder online
                // via Tags. Trifft Album-Ordner und Einzeltitel.
                if let Some(tex) = self.cover_texture(e) {
                    (Some(tex), "media-optical-symbolic")
                } else {
                    // Kein Cover gefunden: nächstbestes – das Interpreten-Foto.
                    // Ordner → Ordnername, Datei → Interpret aus den Tags.
                    let artist = if e.is_dir() {
                        Some(e.name().to_string())
                    } else {
                        scanner::read_track(e.path()).ok().and_then(|t| t.artist)
                    };
                    let photo = artist
                        .filter(|a| !a.trim().is_empty())
                        .and_then(|a| self.library.get_artist_meta(&a).ok().flatten())
                        .and_then(|m| m.image_path)
                        .and_then(|p| gtk::gdk::Texture::from_filename(&p).ok());
                    match photo {
                        Some(tex) => (Some(tex), "avatar-default-symbolic"),
                        None => (None, "media-optical-symbolic"),
                    }
                }
            }
            CtxTarget::Artist(m) => {
                let tex = m
                    .image_path
                    .as_deref()
                    .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
                (tex, "avatar-default-symbolic")
            }
            CtxTarget::Album(m) => {
                let tex = m
                    .cover_path
                    .as_deref()
                    .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
                (tex, "media-optical-symbolic")
            }
        }
    }


    /// Hängt das Cover/Foto an: bei mehreren Bildern ein Karussell mit Punkten,
    /// sonst das einzelne (primäre) Bild wie bisher.
    pub(crate) fn append_cover_or_gallery(
        &self,
        content: &gtk::Box,
        entry: &CtxTarget,
        sender: &ComponentSender<Self>,
        dialog: &adw::Dialog,
    ) {
        let (texture, placeholder) = self.ctx_cover(entry);
        let mut paths = self.ctx_gallery_paths(entry);

        // Langes Drücken oder Rechtsklick auf das Bild: eigenes Cover/Foto wählen.
        let attach_upload = |w: &gtk::Box| {
            let click = gtk::GestureClick::new();
            click.set_button(gtk::gdk::BUTTON_SECONDARY);
            {
                let sender = sender.clone();
                click.connect_pressed(move |g, _, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::UploadCover);
                });
            }
            w.add_controller(click);
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::UploadCover);
                });
            }
            w.add_controller(lp);
        };

        // Aktuelles Primärbild nach vorn, damit das Karussell darauf startet
        // (so wird beim Schließen ohne Blättern nichts ungewollt geändert).
        let primary = match entry {
            CtxTarget::Album(m) => m.cover_path.clone(),
            CtxTarget::Artist(m) => m.image_path.clone(),
            CtxTarget::Fs(_) => None,
        };
        if let Some(pos) = primary.and_then(|p| paths.iter().position(|x| *x == p)) {
            let p = paths.remove(pos);
            paths.insert(0, p);
        }

        if paths.len() > 1 {
            let carousel = adw::Carousel::new();
            carousel.set_halign(gtk::Align::Center);
            for path in &paths {
                let tex = gtk::gdk::Texture::from_filename(path).ok();
                let img = crate::ui::widgets::rounded_image(tex.as_ref(), placeholder, 180);
                carousel.append(&img);
            }
            let dots = adw::CarouselIndicatorDots::new();
            dots.set_carousel(Some(&carousel));

            let gallery = gtk::Box::new(gtk::Orientation::Vertical, 6);
            gallery.set_halign(gtk::Align::Center);
            gallery.append(&carousel);
            gallery.append(&dots);
            content.append(&gallery);
            attach_upload(&gallery);

            // Beim Schließen der Detailansicht das zuletzt im Karussell gezeigte
            // Bild sofort als primäres Cover/Foto übernehmen (gilt dann überall).
            let album_id = match entry {
                CtxTarget::Album(m) => Some((m.artist.clone(), m.album.clone())),
                _ => None,
            };
            let artist_id = match entry {
                CtxTarget::Artist(m) => Some(m.name.clone()),
                _ => None,
            };
            let sender = sender.clone();
            dialog.connect_closed(move |_| {
                let idx = carousel.position().round().max(0.0) as usize;
                let Some(path) = paths.get(idx).cloned() else {
                    return;
                };
                if let Some((artist, album)) = &album_id {
                    sender.input(Msg::SetAlbumCover {
                        artist: artist.clone(),
                        album: album.clone(),
                        path,
                    });
                } else if let Some(name) = &artist_id {
                    sender.input(Msg::SetArtistImage {
                        name: name.clone(),
                        path,
                    });
                }
            });
        } else {
            let cover = crate::ui::widgets::rounded_image(texture.as_ref(), placeholder, 180);
            let cover_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            cover_box.set_halign(gtk::Align::Center);
            cover_box.set_hexpand(false);
            cover_box.append(&cover);
            content.append(&cover_box);
            attach_upload(&cover_box);
        }
    }

    /// Gespeicherte Galerie-Bildpfade eines Ziels (nur existierende Dateien).
    pub(crate) fn ctx_gallery_paths(&self, entry: &CtxTarget) -> Vec<String> {
        let stored = match entry {
            CtxTarget::Artist(m) => self.library.artist_images(&m.name).unwrap_or_default(),
            CtxTarget::Album(m) => {
                self.library.album_images(&m.artist, &m.album).unwrap_or_default()
            }
            CtxTarget::Fs(_) => Vec::new(),
        };
        stored
            .into_iter()
            .filter(|p| std::path::Path::new(p).exists())
            .collect()
    }

    /// Detailzeilen für die "Mehr Infos"-Aufklappung.
    pub(crate) fn ctx_info_lines(&self, target: &CtxTarget) -> Vec<(&'static str, String)> {
        match target {
            CtxTarget::Fs(e) => self.info_lines(e),
            CtxTarget::Artist(m) => {
                let files = self.artist_files(&m.name);
                let mut lines = vec![("Interpret", m.name.clone())];
                // Jahr/Jahre der Alben, je nach Album-Metadaten.
                let year = self.artist_year_range(&m.name);
                let year_shown = year.is_some();
                if let Some((label, value)) = year {
                    lines.push((label, value));
                }
                lines.push(("Kurzübersicht", Self::files_summary(&files, !year_shown)));
                lines
            }
            CtxTarget::Album(m) => {
                let mut lines = Vec::new();
                if !m.artist.is_empty() {
                    lines.push(("Interpret", m.artist.clone()));
                }
                lines.push(("Album", m.album.clone()));
                if let Some(y) = m.year {
                    lines.push(("Jahr", y.to_string()));
                }
                let files = self.album_files(&m.artist, &m.album);
                lines.push(("Kurzübersicht", Self::files_summary(&files, m.year.is_none())));
                lines
            }
        }
    }

    /// "Eigenschaften"-Gruppe des Detailziels (Datei: alle Ebenen; Interpret/Album: passend).
    pub(crate) fn ctx_merkmale(
        &self,
        target: &CtxTarget,
        sender: &ComponentSender<Self>,
    ) -> Option<adw::PreferencesGroup> {
        match target {
            CtxTarget::Fs(e) => self.build_merkmale(e, sender),
            CtxTarget::Artist(m) => Some(self.artist_merkmale(&m.name, sender)),
            CtxTarget::Album(m) => Some(self.album_merkmale(&m.artist, &m.album, sender)),
        }
    }

    /// "Eigenschaften"-Gruppe für einen Interpreten: eine Auswahl auf Interpret-Ebene.
    pub(crate) fn artist_merkmale(
        &self,
        name: &str,
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().build();
        let expander = adw::ExpanderRow::builder().title("Eigenschaften").build();

        let (eff, src) = self.library.resolve_category(Some(name), None, "");
        let eff_label = Category::from_str(&eff).unwrap_or(Category::DEFAULT).label();
        let src_label = if src == "artist" { "Interpret" } else { "Standard" };
        expander.set_subtitle(&format!("{eff_label} (von: {src_label})"));

        let cur = self.library.get_category("artist", name).ok().flatten();
        self.add_category_row(
            &expander,
            sender,
            &format!("Interpret: {name}"),
            "artist",
            name.to_string(),
            cur,
        );

        group.add(&expander);
        group
    }

    /// "Eigenschaften"-Gruppe für ein Album: Album-Ebene plus geerbte Interpret-Ebene.
    pub(crate) fn album_merkmale(
        &self,
        artist: &str,
        album: &str,
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().build();
        let expander = adw::ExpanderRow::builder().title("Eigenschaften").build();

        let (eff, src) = self.library.resolve_category(Some(artist), Some(album), "");
        let eff_label = Category::from_str(&eff).unwrap_or(Category::DEFAULT).label();
        let src_label = match src {
            "album" => "Album",
            "artist" => "Interpret",
            _ => "Standard",
        };
        expander.set_subtitle(&format!("{eff_label} (von: {src_label})"));

        // Album-Ebene
        let key = category::album_key(artist, album);
        let cur = self.library.get_category("album", &key).ok().flatten();
        self.add_category_row(
            &expander,
            sender,
            &format!("Album: {album}"),
            "album",
            key,
            cur,
        );
        // Interpret-Ebene (geerbt)
        if !artist.is_empty() {
            let cur = self.library.get_category("artist", artist).ok().flatten();
            self.add_category_row(
                &expander,
                sender,
                &format!("Interpret: {artist}"),
                "artist",
                artist.to_string(),
                cur,
            );
        }

        group.add(&expander);
        group
    }

    /// Kurzübersicht über eine Dateimenge: „N Alben - M Lieder - 2001–2010".
    /// Kurzübersicht „N Alben - M Lieder[ - Jahr/Bereich]". Das Jahr wird nur
    /// angehängt, wenn `with_year` gesetzt ist – sobald eine eigene „Jahr"/„Jahre"-
    /// Zeile angezeigt wird, entfällt es hier (Dopplung vermeiden).
    pub(crate) fn files_summary(files: &[PathBuf], with_year: bool) -> String {
        let songs = files.len();
        let mut albums = std::collections::HashSet::new();
        let mut min_year: Option<u32> = None;
        let mut max_year: Option<u32> = None;
        for f in files {
            let (album, year) = scanner::read_album_year(f);
            if let Some(a) = album {
                albums.insert(a);
            }
            if let Some(y) = year {
                min_year = Some(min_year.map_or(y, |m| m.min(y)));
                max_year = Some(max_year.map_or(y, |m| m.max(y)));
            }
        }

        let mut value = String::new();
        let n = albums.len();
        if n > 0 {
            value.push_str(&format!("{n} {} - ", if n == 1 { "Album" } else { "Alben" }));
        }
        value.push_str(&format!("{songs} {}", if songs == 1 { "Lied" } else { "Lieder" }));
        if with_year {
            if let (Some(a), Some(b)) = (min_year, max_year) {
                let span = if a == b {
                    a.to_string()
                } else {
                    format!("{a}\u{2013}{b}")
                };
                value.push_str(&format!(" - {span}"));
            }
        }
        value
    }

    pub(crate) fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    /// Beschafft ein Cover als Textur. Für einen **Ordner** das Ordner-Cover
    /// (= Albumbild); für eine **Einzeldatei** bewusst **kein** Ordnerbild, damit
    /// ein Titel kein fremdes Cover aus einem geteilten Ordner erbt – stattdessen
    /// das eingebettete Bild der Datei bzw. das online zugeordnete Album-Cover.
    /// `None`, wenn nichts Passendes gefunden wird.
    pub(crate) fn cover_texture(&self, entry: &FsEntry) -> Option<gtk::gdk::Texture> {
        if entry.is_dir() {
            if let Some(path) = cover::find_cover_file(entry.path()) {
                if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                    return Some(texture);
                }
            }
        }

        let audio = if entry.is_dir() {
            std::fs::read_dir(entry.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| scanner::is_audio(p))
                .min()
        } else {
            Some(entry.path().clone())
        };

        if let Some(audio) = &audio {
            if let Some(bytes) = cover::embedded_cover(audio) {
                if let Ok(tex) =
                    gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(bytes.as_slice()))
                {
                    return Some(tex);
                }
            }
        }

        // Zuletzt: online geladenes Cover aus dem Cache (über die Tags zugeordnet).
        let track = scanner::read_track(audio.as_ref()?).ok()?;
        let (artist, album) = (track.artist?, track.album?);
        let meta = self.library.get_album_meta(&artist, &album).ok().flatten()?;
        let path = meta.cover_path?;
        gtk::gdk::Texture::from_filename(&path).ok()
    }

    /// Detailzeilen für die "Mehr Infos"-Aufklappung.
    pub(crate) fn info_lines(&self, entry: &FsEntry) -> Vec<(&'static str, String)> {
        let mut lines = Vec::new();
        if entry.is_dir() {
            // Als Album/Interpret erkannte Ordner zeigen passende Infos inkl. Jahr.
            let mut year_shown = false;
            match self.fs_music_kind(entry) {
                Some(FsKind::Album { artist, album }) => {
                    if !artist.is_empty() {
                        lines.push(("Interpret", artist.clone()));
                    }
                    lines.push(("Album", album.clone()));
                    if let Some(y) = self
                        .library
                        .get_album_meta(&artist, &album)
                        .ok()
                        .flatten()
                        .and_then(|m| m.year)
                    {
                        lines.push(("Jahr", y.to_string()));
                        year_shown = true;
                    }
                }
                Some(FsKind::Artist(name)) => {
                    lines.push(("Interpret", name.clone()));
                    if let Some((label, value)) = self.artist_year_range(&name) {
                        lines.push((label, value));
                        year_shown = true;
                    }
                }
                None => {}
            }
            let files = self.entry_files(entry);
            lines.push(("Kurzübersicht", Self::files_summary(&files, !year_shown)));
        } else {
            match scanner::read_track(entry.path()) {
                Ok(t) => {
                    lines.push(("Titel", t.title));
                    // Interpret/Album für die Jahres-Auflösung merken (werden
                    // beim Anzeigen verbraucht).
                    let (artist, album) = (t.artist.clone(), t.album.clone());
                    if let Some(a) = t.artist {
                        lines.push(("Interpret", a));
                    }
                    if let Some(al) = t.album {
                        lines.push(("Album", al));
                    }
                    if let Some(d) = t.duration_ms {
                        lines.push(("Dauer", fmt_duration(d)));
                    }
                    // Jahr (aus den Album-Metadaten) direkt unter der Dauer.
                    if let (Some(artist), Some(album)) = (artist, album) {
                        if let Some(y) = self
                            .library
                            .get_album_meta(&artist, &album)
                            .ok()
                            .flatten()
                            .and_then(|m| m.year)
                        {
                            lines.push(("Jahr", y.to_string()));
                        }
                    }
                }
                Err(_) => {}
            }

            // Per Fingerprint (AcoustID) erkannte Vorschläge – nur Anzeige,
            // wird nicht in die Datei geschrieben.
            if let Ok(Some(m)) = self
                .library
                .get_track_meta(&entry.path().to_string_lossy())
            {
                if m.status == "matched" {
                    if let Some(t) = m.title {
                        lines.push(("Erkannt (Titel)", t));
                    }
                    if let Some(a) = m.artist {
                        lines.push(("Erkannt (Interpret)", a));
                    }
                    if let Some(al) = m.album {
                        lines.push(("Erkannt (Album)", al));
                    }
                }
            }
        }
        lines
    }
}
