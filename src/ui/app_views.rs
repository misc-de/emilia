//! Ansichten und Daten-Helfer: Ordner/Album/Interpret laden und gruppieren, die
//! Unterseiten (Interpret → Alben → Titel) bauen, sowie die Kontext-/Detail-
//! Helfer (ctx_*) und die Cover-Auflösung. Aus app.rs herausgelöst – reine
//! Umordnung, kein Funktionswechsel.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category;
use crate::core::db::Library;
use crate::core::{cover, scanner};
use crate::i18n::{gettext, ngettext_n};
use crate::model::{ArtistMeta, Track};
use crate::ui::app::{
    album_subtitle, cover_widget, duration_label, find_scroller, fmt_duration, most_common_artist,
    read_entries, App, Cmd, CtxTarget, FsKind, Msg,
};
use crate::ui::enrich::enrich_worker;
use crate::ui::fs_row::FsEntry;

/// Wie ein in einer Album-Titelliste angetippter Titel abgespielt wird.
#[derive(Clone)]
enum AlbumPlay {
    /// Interpreten-Kontext (Interpret → Album): nur dessen Album-Titel.
    Artist(String),
    /// Alben-Übersicht: alle Titel des Albumnamens (Interpret egal).
    Name(String),
    /// Ordner-Inhalt (Hörbuch/Konzert): genau die Dateien dieses Ordners.
    Folder(String),
}

/// Album-Name ohne CD-/Disc-Suffix, damit Mehr-CD-Alben zusammenfallen:
/// „… Disc 2", „… CD 1", „… Cd 2 v 7", „… CD3" → der gemeinsame Basistitel.
fn album_base(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let clean = |w: &str| {
        w.to_lowercase()
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string()
    };
    const MARKERS: [&str; 8] = ["disc", "disk", "cd", "teil", "part", "folge", "vol", "volume"];
    let is_marker = |w: &str| {
        let c = clean(w);
        MARKERS.iter().any(|m| {
            c == *m
                || (c.starts_with(m) && c.len() > m.len() && c[m.len()..].chars().all(|d| d.is_ascii_digit()))
        })
    };
    const CONNECTORS: [&str; 6] = ["v", "von", "of", "x", "u", "und"];
    let is_suffix_tok = |w: &str| {
        let c = clean(w);
        c.is_empty()
            || c.chars().all(|d| d.is_ascii_digit())
            || CONNECTORS.contains(&c.as_str())
            || is_marker(w)
    };
    // Erste Marker-Position, ab der bis zum Ende nur noch Suffix-Tokens stehen.
    let cut = (0..words.len()).find(|&i| is_marker(words[i]) && words[i..].iter().all(|w| is_suffix_tok(w)));
    let base = match cut {
        Some(i) => words[..i].join(" "),
        None => name.trim().to_string(),
    };
    let base = base.trim_matches(|c: char| c == '-' || c == ':' || c.is_whitespace());
    if base.is_empty() {
        name.trim().to_string()
    } else {
        base.to_string()
    }
}

/// Disc-Nummer aus einem Ordner-Segment wie „CD2", „CD 2", „Disc 03", „Teil 2".
/// Nur wenn das Segment mit einem Disc-Schlüsselwort **beginnt** und darauf
/// (ggf. nach Trennzeichen) Ziffern folgen – so lösen „Greatest Hits" o. Ä.
/// nichts aus. Sonst `None`.
fn disc_from_segment(seg: &str) -> Option<u32> {
    let s = seg.trim().to_ascii_lowercase();
    const MARKERS: [&str; 6] = ["cd", "disc", "disk", "teil", "part", "folge"];
    for kw in MARKERS {
        if let Some(rest) = s.strip_prefix(kw) {
            let digits: String = rest
                .trim_start_matches(|c: char| matches!(c, ' ' | '_' | '.' | '#' | '-'))
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

/// Effektive Disc-Nummer eines Titels: bevorzugt das `disc_no`-Tag, sonst aus
/// einem CD-/Disc-**Unterordner** des Pfads abgeleitet (Hörbücher ohne Disc-Tag),
/// sonst 1. Es zählen nur Verzeichnis-Segmente, nicht der Dateiname.
pub(crate) fn track_disc(t: &Track) -> u32 {
    if let Some(d) = t.disc_no {
        return d;
    }
    std::path::Path::new(&t.path)
        .parent()
        .into_iter()
        .flat_map(|d| d.components())
        .filter_map(|c| c.as_os_str().to_str())
        .filter_map(disc_from_segment)
        .last()
        .unwrap_or(1)
}

/// Häufigster Album-Basistitel einer Titelmenge (für den Anzeigetitel eines
/// als Album zusammengefassten Unterordners).
fn most_common_album_base(tracks: &[&Track]) -> Option<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for t in tracks {
        if let Some(al) = t.album.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            *counts.entry(album_base(al)).or_default() += 1;
        }
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(b, _)| b)
}

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
                        tracing::warn!("Library scan failed: {e}");
                    }
                }
                Err(e) => tracing::error!("Database unavailable for scan: {e}"),
            }
            Cmd::ScanDone { then_enrich }
        });
    }

    /// Startet die Online-Anreicherung im Hintergrund. `scan_first`: zuvor noch die
    /// Tags einlesen (beim manuellen Abruf) – beim automatischen Lauf entfällt das,
    /// weil der lokale Scan bereits durchlief. Die Audiodateien werden dabei nur
    /// gelesen, niemals verändert. Dauerhaft erfolglose Einträge (≥ 3 Versuche)
    /// werden in beiden Fällen übersprungen.
    pub(crate) fn run_enrich(&mut self, sender: &ComponentSender<Self>, scan_first: bool) {
        let Some(root) = self.root_dir.clone() else {
            self.toast(&gettext("No music folder set – please choose one in the settings"));
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
            gettext("Reading library …")
        } else {
            gettext("Searching for cover & metadata …")
        };
        sender
            .spawn_command(move |out| enrich_worker(root, key, fkey, cancel, scan_first, &out));
    }

    /// Lädt die Interpreten-Übersicht aus der DB in die Factory (inkl. Foto).
    /// Fehlt das Interpretenfoto, wird ersatzweise ein Album-Cover eingebunden.
    pub(crate) fn reload_artists(&mut self) {
        let mut artists = self.library.artists_overview().unwrap_or_default();
        self.artist_count = artists.len();
        for a in &mut artists {
            if a.image_path.as_deref().map_or(true, |p| p.trim().is_empty()) {
                a.image_path = self.artist_album_cover(&a.name);
            }
        }
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
            // Nur Titel, bei denen dieser Interpret der **Haupt-Interpret** ist,
            // bilden ein Album. Reine Gast-/Feature-Titel (Namensnennung) fließen
            // NICHT in den Album-Aufbau ein – sie zählen als Einzellieder.
            let (own_tracks, guest_tracks): (Vec<Track>, Vec<Track>) =
                mine.into_iter().partition(|t| {
                    t.artist.as_deref().is_some_and(|a| {
                        crate::core::artist::split_artists(a)
                            .first()
                            .is_some_and(|p| crate::core::artist::norm_key(p) == target)
                    })
                });
            // Album nur ab zwei eigenen Titeln; sonst zählen sie als Einzellieder.
            if own_tracks.len() >= 2 {
                let display_artist = most_common_artist(&own_tracks);
                albums.push((album, display_artist, own_tracks));
            } else {
                singles.extend(own_tracks);
            }
            singles.extend(guest_tracks);
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

    /// Alle Titel mit diesem Albumnamen – **interpretenübergreifend** (passend
    /// zur Alben-Übersicht, die rein nach Albumnamen gruppiert). Sortiert nach
    /// Disc-/Tracknummer, dann Pfad.
    pub(crate) fn album_tracks_by_name(&self, album: &str) -> Vec<Track> {
        let target = album.to_lowercase();
        let mut tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.album
                    .as_deref()
                    .is_some_and(|a| a.to_lowercase() == target)
            })
            .collect();
        tracks.sort_by(|a, b| {
            // Disc aus Tag oder CD-Unterordner (Hörbücher ohne Disc-Tag), dann
            // Tracknummer, dann Pfad.
            track_disc(a)
                .cmp(&track_disc(b))
                .then(a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
                .then_with(|| a.path.cmp(&b.path))
        });
        tracks
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
                    .title(&gettext("No tracks"))
                    .description(&gettext("There are no songs for this artist in the library."))
                    .build(),
            );
        }

        // --- Alben zuerst ---
        if !album_groups.is_empty() {
            let n = album_groups.len();
            let group = adw::PreferencesGroup::builder()
                .title(&gettext("Albums"))
                .description(ngettext_n("{n} album", "{n} albums", n as u32))
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
                play.set_tooltip_text(Some(&gettext("Play album")));
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
                .title(&gettext("Singles"))
                .description(ngettext_n("{n} song", "{n} songs", n as u32))
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
        self.render_album_tracks(sender, tracks, name, album, AlbumPlay::Artist(name.to_string()));
    }

    /// Album aus der Alben-Übersicht: **alle** Titel dieses Albumnamens
    /// (Interpret egal). Tippen auf einen Titel spielt das ganze Album ab hier.
    pub(crate) fn open_album_by_name(&self, sender: &ComponentSender<Self>, album: &str) {
        let tracks = self.album_tracks_by_name(album);
        self.render_album_tracks(sender, tracks, "", album, AlbumPlay::Name(album.to_string()));
    }

    /// Titel eines Ordners in Abspielreihenfolge (CD/Disc, Tracknummer, Pfad).
    /// Grundlage für die Titelliste eines als Album dargestellten Ordners.
    pub(crate) fn folder_tracks_ordered(&self, folder: &str) -> Vec<Track> {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        let mut tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.path.starts_with(&prefix))
            .collect();
        tracks.sort_by(|a, b| {
            // Disc aus Tag oder CD-Unterordner (Hörbücher ohne Disc-Tag), dann
            // Tracknummer, dann Pfad.
            track_disc(a)
                .cmp(&track_disc(b))
                .then(a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
                .then_with(|| a.path.cmp(&b.path))
        });
        tracks
    }

    /// Tippen auf ein als Album dargestelltes **Ordner**-Hörbuch/-Konzert: listet
    /// dessen Titel auf. Tippen auf einen Titel spielt den Ordner ab dort.
    pub(crate) fn open_folder_tracks(&self, sender: &ComponentSender<Self>, folder: &str) {
        let tracks = self.folder_tracks_ordered(folder);
        let refs: Vec<&Track> = tracks.iter().collect();
        let album = most_common_album_base(&refs).unwrap_or_else(|| {
            std::path::Path::new(folder)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let name = most_common_artist(&tracks);
        self.render_album_tracks(sender, tracks, &name, &album, AlbumPlay::Folder(folder.to_string()));
    }

    /// Gemeinsame Darstellung einer Album-Titelliste. `play` bestimmt, wie ein
    /// angetippter Titel abgespielt wird (interpretenbezogen oder nach Albumname).
    fn render_album_tracks(
        &self,
        sender: &ComponentSender<Self>,
        tracks: Vec<Track>,
        name: &str,
        album: &str,
        play: AlbumPlay,
    ) {
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

        // Hörbuch? Dann steht oben der Titel statt der Anzahl der Lieder.
        let is_audiobook = {
            use crate::core::category::Area;
            let areas = match &play {
                AlbumPlay::Folder(f) => self.library.folder_areas(f),
                _ => self.library.album_areas(&display_artist, album),
            };
            areas.contains(&Area::Audiobooks)
        };

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Zeile **über** der Überschrift: bei Hörbüchern der Titel, sonst die
        // Anzahl der Lieder (+ Jahr).
        let header_text = if is_audiobook {
            album.to_string()
        } else {
            album_subtitle(year, tracks.len())
        };
        if !header_text.trim().is_empty() {
            let lbl = gtk::Label::builder()
                .label(gtk::glib::markup_escape_text(&header_text).as_str())
                .xalign(0.0)
                .wrap(true)
                .margin_start(4)
                .build();
            lbl.add_css_class(if is_audiobook { "title-4" } else { "dim-label" });
            content.append(&lbl);
        }

        // Vorhandene Discs ermitteln (None gilt als CD 1). Mehr als eine → die
        // Titel werden nach „CD 1" / „CD 2" … getrennt dargestellt.
        let mut discs: Vec<u32> = tracks.iter().map(track_disc).collect();
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
                let play = play.clone();
                let album = album.to_string();
                let path = path.clone();
                row.connect_activated(move |_| {
                    sender.input(match &play {
                        AlbumPlay::Artist(a) => Msg::PlayAlbumTrack {
                            artist: a.clone(),
                            album: album.clone(),
                            path: path.clone(),
                        },
                        AlbumPlay::Name(al) => Msg::PlayAlbumByNameTrack {
                            album: al.clone(),
                            path: path.clone(),
                        },
                        AlbumPlay::Folder(f) => Msg::PlayFolderTrack {
                            folder: f.clone(),
                            path: path.clone(),
                        },
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
            // Mehrere CDs → je „CD 1" / „CD 2" … eine Gruppe (die Songzahl bzw.
            // der Titel steht bereits oben über den Abschnitten).
            for disc in &discs {
                let group = adw::PreferencesGroup::builder().title(format!("CD {disc}")).build();
                for t in tracks.iter().filter(|t| track_disc(t) == *disc) {
                    group.add(&make_row(t));
                }
                content.append(&group);
            }
        } else {
            // Einzel-CD: bei Hörbüchern ohne wiederholte Titel-Überschrift (steht
            // schon oben), sonst der Albumname als Gruppentitel.
            let group = if is_audiobook {
                adw::PreferencesGroup::new()
            } else {
                adw::PreferencesGroup::builder()
                    .title(gtk::glib::markup_escape_text(album).as_str())
                    .build()
            };
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

    /// Wandelt rohe Bereichs-Einträge (Konzerte/Hörbücher) in eine Liste aus
    /// **Alben und Einzelstücken** um: „album"/„track" bleiben; ein markierter
    /// „folder" wird in seine Alben und losen Titel aufgelöst; „artist" entfällt.
    /// Dedupliziert nach (scope, key), alphabetisch nach Titel.
    pub(crate) fn expand_area_items(
        &self,
        raw: Vec<(String, String, String, bool)>,
    ) -> Vec<(String, String, String, bool)> {
        use std::collections::HashSet;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut out: Vec<(String, String, String, bool)> = Vec::new();
        for (scope, key, title, is_dir) in raw {
            let expanded = match scope.as_str() {
                "album" | "track" => vec![(scope, key, title, is_dir)],
                "folder" => self.folder_albums_and_tracks(&key),
                _ => vec![], // „artist" o. Ä. nicht als solchen listen
            };
            for e in expanded {
                if seen.insert((e.0.clone(), e.1.clone())) {
                    out.push(e);
                }
            }
        }
        out.sort_by(|a, b| a.2.to_lowercase().cmp(&b.2.to_lowercase()));
        out
    }

    /// Löst einen Ordner in **Alben** und **Einzelstücke** auf:
    /// * Jeder unmittelbare **Unterordner** ist ein Album (Mehr-CD-Inhalte darin
    ///   fallen zu einem Eintrag zusammen; Titel = häufigster Album-Tag ohne
    ///   CD/Disc-Suffix, sonst Ordnername).
    /// * Dateien **direkt** im Ordner werden nach **Album-Tag** zu Album-Einträgen
    ///   gruppiert (dedupliziert mit bereits als Album markierten Konzerten);
    ///   **keine** Einzeldateien aus einem Album.
    /// * Nur Dateien **ohne** Album-Tag sind lose **Einzelstücke**.
    pub(crate) fn folder_albums_and_tracks(&self, dir: &str) -> Vec<(String, String, String, bool)> {
        use crate::core::category::album_key;
        use std::collections::BTreeMap;

        let base = dir.trim_end_matches('/');
        let prefix = format!("{base}/");
        let tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.path.starts_with(&prefix))
            .collect();

        // Nach unmittelbarem Unterordner gruppieren; ohne Unterordner = lose Datei.
        let mut subfolders: BTreeMap<String, Vec<&Track>> = BTreeMap::new();
        let mut loose: Vec<&Track> = Vec::new();
        for t in &tracks {
            let rel = &t.path[prefix.len()..];
            match rel.find('/') {
                Some(i) => subfolders.entry(rel[..i].to_string()).or_default().push(t),
                None => loose.push(t),
            }
        }

        let mut out = Vec::new();
        // Jeder Unterordner = ein Album (alle CDs/Teile zusammen).
        for (sub, grp) in &subfolders {
            let key = format!("{base}/{sub}");
            let title = most_common_album_base(grp).unwrap_or_else(|| sub.clone());
            out.push(("folder".to_string(), key, title, true));
        }
        // Lose Dateien: nach **Album-Tag** zu einem Album-Eintrag gruppieren – kein
        // Einzeltitel aus einem Album. Der Schlüssel nutzt den häufigsten
        // Haupt-Interpreten (feat. abgetrennt) wie `albums_overview`, damit ein
        // bereits als Album markiertes Konzert dedupliziert wird.
        use crate::core::artist::primary_artist;
        let mut by_album: BTreeMap<String, Vec<&Track>> = BTreeMap::new();
        for t in &loose {
            match t.album.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                Some(al) => by_album.entry(al.to_string()).or_default().push(t),
                None => {
                    let title = if t.title.trim().is_empty() {
                        std::path::Path::new(&t.path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    } else {
                        t.title.clone()
                    };
                    out.push(("track".to_string(), t.path.clone(), title, false));
                }
            }
        }
        for (al, grp) in &by_album {
            let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for t in grp {
                *counts
                    .entry(primary_artist(t.artist.as_deref().unwrap_or("")))
                    .or_default() += 1;
            }
            let artist = counts
                .into_iter()
                .max_by_key(|(_, n)| *n)
                .map(|(a, _)| a)
                .unwrap_or_default();
            out.push(("album".to_string(), album_key(&artist, al), al.clone(), false));
        }
        out
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
                "the artist",
                name.clone(),
                Some("Also applies to this artist's albums and tracks."),
                "artist",
                name,
            )),
            FsKind::Album { artist, album } => {
                let key = category::album_key(&artist, &album);
                Some((
                    "the album",
                    album,
                    Some("Also applies to this album's tracks."),
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
            [y] => Some(("Year", y.to_string())),
            _ => Some(("Years", format!("{} – {}", years[0], years[years.len() - 1]))),
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
                // Foto, sonst ersatzweise ein Album-Cover des Interpreten.
                let img = m.image_path.clone().or_else(|| self.artist_album_cover(&m.name));
                let tex = img.and_then(|p| gtk::gdk::Texture::from_filename(&p).ok());
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
    pub(crate) fn ctx_info_lines(&self, target: &CtxTarget) -> Vec<(String, String)> {
        match target {
            CtxTarget::Fs(e) => self.info_lines(e),
            CtxTarget::Artist(m) => {
                let files = self.artist_files(&m.name);
                let mut lines = vec![(gettext("Artist"), m.name.clone())];
                // Jahr/Jahre der Alben, je nach Album-Metadaten.
                let year = self.artist_year_range(&m.name);
                let year_shown = year.is_some();
                if let Some((label, value)) = year {
                    lines.push((gettext(label), value));
                }
                lines.push((gettext("Collection"), Self::files_summary(&files, !year_shown)));
                lines
            }
            CtxTarget::Album(m) => {
                let mut lines = Vec::new();
                if !m.artist.is_empty() {
                    lines.push((gettext("Artist"), m.artist.clone()));
                }
                lines.push((gettext("Album"), m.album.clone()));
                let files = self.album_files(&m.artist, &m.album);
                if let Some(g) = Self::first_genre(&files) {
                    lines.push((gettext("Genre"), g));
                }
                if let Some(y) = m.year {
                    lines.push((gettext("Year"), y.to_string()));
                }
                lines.push((gettext("Collection"), Self::files_summary(&files, m.year.is_none())));
                lines
            }
        }
    }

    /// „Eigenschaften"-Gruppe des Detailziels: Mehrfachauswahl der Bereiche, in
    /// denen der Inhalt erscheint (leer = ausgeblendet). Festgelegt wird auf der
    /// passenden Ebene (Titel/Album/Interpret); die Vererbung übernimmt
    /// `resolve_areas`.
    pub(crate) fn ctx_merkmale(
        &self,
        target: &CtxTarget,
        sender: &ComponentSender<Self>,
    ) -> Option<adw::PreferencesGroup> {
        use crate::core::category::{album_key, Area};
        let (scope, key, effective): (&'static str, String, Vec<Area>) = match target {
            CtxTarget::Artist(m) => ("artist", m.name.clone(), self.library.artist_areas(&m.name)),
            CtxTarget::Album(m) => (
                "album",
                album_key(&m.artist, &m.album),
                self.library.album_areas(&m.artist, &m.album),
            ),
            CtxTarget::Fs(e) if !e.is_dir() => {
                let track = scanner::read_track(e.path()).ok()?;
                let path = e.path().to_string_lossy().into_owned();
                let eff =
                    self.library
                        .resolve_areas(track.artist.as_deref(), track.album.as_deref(), &path);
                ("track", path, eff)
            }
            CtxTarget::Fs(e) => match self.fs_music_kind(e) {
                Some(FsKind::Album { artist, album }) => (
                    "album",
                    album_key(&artist, &album),
                    self.library.album_areas(&artist, &album),
                ),
                Some(FsKind::Artist(name)) => {
                    ("artist", name.clone(), self.library.artist_areas(&name))
                }
                // Generischer Ordner (z. B. erste Ebene): Ordner-Ebene, vererbt
                // an alles darunter.
                None => {
                    let path = e.path().to_string_lossy().into_owned();
                    let eff = self.library.folder_areas(&path);
                    ("folder", path, eff)
                }
            },
        };
        Some(self.build_area_group(scope, key, &effective, sender))
    }

    /// Bereichs-Auswahl (ein Schalter je Bereich) für eine Ebene. Alle Schalter
    /// aus = ausgeblendet.
    fn build_area_group(
        &self,
        scope: &'static str,
        key: String,
        effective: &[crate::core::category::Area],
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesGroup {
        use crate::core::category::{areas_value, Area};
        use std::cell::RefCell;
        use std::rc::Rc;

        // Nur Bereiche zeigen, deren Menüpunkt sichtbar ist (Hörbücher hat keinen
        // eigenen Menüpunkt und bleibt immer wählbar). Werte ausgeblendeter
        // Bereiche bleiben im Zustand erhalten und werden nicht angetastet.
        let visible_areas: Rc<Vec<Area>> = Rc::new(
            Area::ALL
                .iter()
                .copied()
                .filter(|a| a.section().map_or(true, |s| !self.hidden_sections.contains(s)))
                .collect(),
        );
        let group = adw::PreferencesGroup::builder().build();
        let expander = adw::ExpanderRow::builder().title(&gettext("Properties")).build();
        let active: Vec<String> = visible_areas
            .iter()
            .filter(|a| effective.contains(a))
            .map(|a| gettext(a.label()))
            .collect();
        let subtitle = if active.is_empty() {
            gettext("Hidden")
        } else {
            active.join(", ")
        };
        expander.set_subtitle(&subtitle);

        let state = Rc::new(RefCell::new(effective.to_vec()));
        let syncing = Rc::new(std::cell::Cell::new(false));

        // „Ausblenden": alle sichtbaren Bereiche aus → überall unsichtbar.
        let hide_row = adw::SwitchRow::builder()
            .title(&gettext("Hide"))
            .active(!visible_areas.iter().any(|a| effective.contains(a)))
            .build();
        expander.add_row(&hide_row);

        // Ein Schalter je sichtbarem Bereich.
        let area_rows: Rc<Vec<(Area, adw::SwitchRow)>> = Rc::new(
            visible_areas
                .iter()
                .map(|&area| {
                    let row = adw::SwitchRow::builder()
                        .title(&gettext(area.label()))
                        .active(effective.contains(&area))
                        .build();
                    expander.add_row(&row);
                    (area, row)
                })
                .collect(),
        );

        // Ausblenden: entfernt alle sichtbaren Bereiche bzw. setzt die sichtbaren
        // Standardbereiche und gleicht die Schalter an.
        {
            let (sender, key, state, syncing, area_rows, visible_areas) = (
                sender.clone(),
                key.clone(),
                state.clone(),
                syncing.clone(),
                area_rows.clone(),
                visible_areas.clone(),
            );
            hide_row.connect_active_notify(move |r| {
                if syncing.get() {
                    return;
                }
                {
                    let mut s = state.borrow_mut();
                    if r.is_active() {
                        s.retain(|a| !visible_areas.contains(a));
                    } else {
                        for a in Area::DEFAULT {
                            if visible_areas.contains(&a) && !s.contains(&a) {
                                s.push(a);
                            }
                        }
                    }
                }
                syncing.set(true);
                for (area, sw) in area_rows.iter() {
                    sw.set_active(state.borrow().contains(area));
                }
                syncing.set(false);
                sender.input(Msg::SetAreas {
                    scope,
                    key: key.clone(),
                    value: areas_value(&state.borrow()),
                });
            });
        }

        // Bereichs-Schalter: Zustand anpassen und „Ausblenden" spiegeln.
        for (area, row) in area_rows.iter() {
            let area = *area;
            let (sender, key, state, syncing, hide_row, visible_areas) = (
                sender.clone(),
                key.clone(),
                state.clone(),
                syncing.clone(),
                hide_row.clone(),
                visible_areas.clone(),
            );
            row.connect_active_notify(move |r| {
                if syncing.get() {
                    return;
                }
                {
                    let mut s = state.borrow_mut();
                    if r.is_active() {
                        if !s.contains(&area) {
                            s.push(area);
                        }
                    } else {
                        s.retain(|a| *a != area);
                    }
                }
                syncing.set(true);
                let hidden = !visible_areas.iter().any(|a| state.borrow().contains(a));
                hide_row.set_active(hidden);
                syncing.set(false);
                sender.input(Msg::SetAreas {
                    scope,
                    key: key.clone(),
                    value: areas_value(&state.borrow()),
                });
            });
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
            value.push_str(&format!("{} - ", ngettext_n("{n} album", "{n} albums", n as u32)));
        }
        value.push_str(&ngettext_n("{n} song", "{n} songs", songs as u32));
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

    /// Erstes gesetztes Genre einer Dateimenge (für die Album-Anzeige). Alben
    /// sind in der Regel genre-einheitlich, daher genügt der erste Treffer.
    fn first_genre(files: &[PathBuf]) -> Option<String> {
        files
            .iter()
            .find_map(|f| scanner::read_genre_composer(f).0)
    }

    /// Detailzeilen für die "Mehr Infos"-Aufklappung.
    pub(crate) fn info_lines(&self, entry: &FsEntry) -> Vec<(String, String)> {
        let mut lines = Vec::new();
        if entry.is_dir() {
            // Als Album/Interpret erkannte Ordner zeigen passende Infos inkl. Jahr.
            let files = self.entry_files(entry);
            let mut year_shown = false;
            match self.fs_music_kind(entry) {
                Some(FsKind::Album { artist, album }) => {
                    if !artist.is_empty() {
                        lines.push((gettext("Artist"), artist.clone()));
                    }
                    lines.push((gettext("Album"), album.clone()));
                    if let Some(g) = Self::first_genre(&files) {
                        lines.push((gettext("Genre"), g));
                    }
                    if let Some(y) = self
                        .library
                        .get_album_meta(&artist, &album)
                        .ok()
                        .flatten()
                        .and_then(|m| m.year)
                    {
                        lines.push((gettext("Year"), y.to_string()));
                        year_shown = true;
                    }
                }
                Some(FsKind::Artist(name)) => {
                    lines.push((gettext("Artist"), name.clone()));
                    if let Some((label, value)) = self.artist_year_range(&name) {
                        lines.push((gettext(label), value));
                        year_shown = true;
                    }
                }
                None => {}
            }
            lines.push((gettext("Collection"), Self::files_summary(&files, !year_shown)));
        } else {
            match scanner::read_track(entry.path()) {
                Ok(t) => {
                    lines.push((gettext("Title"), t.title));
                    // Interpret/Album für die Jahres-Auflösung merken (werden
                    // beim Anzeigen verbraucht).
                    let (artist, album) = (t.artist.clone(), t.album.clone());
                    // Genre + Komponist aus den Datei-Tags (nur Anzeige). Der
                    // Komponist wird immer gezeigt, wenn er getaggt ist (relevant
                    // für Klassik/Hörspiele); das Genre, wann immer vorhanden.
                    let (genre, composer) = scanner::read_genre_composer(entry.path());
                    if let Some(a) = t.artist {
                        lines.push((gettext("Artist"), a));
                    }
                    if let Some(c) = composer {
                        lines.push((gettext("Composer"), c));
                    }
                    if let Some(al) = t.album {
                        lines.push((gettext("Album"), al));
                    }
                    if let Some(g) = genre {
                        lines.push((gettext("Genre"), g));
                    }
                    if let Some(d) = t.duration_ms {
                        lines.push((gettext("Duration"), fmt_duration(d)));
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
                            lines.push((gettext("Year"), y.to_string()));
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
                        lines.push((gettext("Detected (title)"), t));
                    }
                    if let Some(a) = m.artist {
                        lines.push((gettext("Detected (artist)"), a));
                    }
                    if let Some(al) = m.album {
                        lines.push((gettext("Detected (album)"), al));
                    }
                }
            }
        }
        lines
    }
}
