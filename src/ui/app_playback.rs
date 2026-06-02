//! Wiedergabe: Warteschlange, Play/Pause/Next/Prev, Resume-Logik und der
//! laufende Equalizer. Aus app.rs herausgelöst – reine Umordnung, kein
//! Funktionswechsel; die Methoden bleiben inhärente `impl App`-Methoden.

use std::path::PathBuf;

use relm4::gtk;

use crate::core::scanner;
use crate::core::webdav::{self, Creds};
use crate::model::Track;
use crate::ui::app::{guarded_resume, ActiveSource, App, Msg, PlaySession, RemoteTrack};
use crate::ui::fs_row::{FsEntry, FsInput};

impl App {
    /// Aktualisiert die Queue-Markierung aller sichtbaren Dateizeilen.
    pub(crate) fn refresh_queue_icons(&mut self) {
        let queue = self.queue.clone();
        // Aktuell laufender Titel (für die Play-Markierung).
        let active_path = self.queue.get(self.queue_pos).cloned();
        // Entfernte Wiedergabe: aktiver Eintrag wird über den rel-Pfad markiert.
        let active_rel = if self.playing_remote {
            self.remote_queue.get(self.remote_pos).map(|t| t.rel_path.clone())
        } else {
            None
        };
        let states: Vec<(usize, bool, bool)> = {
            let guard = self.entries.guard();
            (0..guard.len())
                .filter_map(|i| {
                    guard.get(i).map(|r| {
                        let is_file = !r.entry.is_dir();
                        match r.entry.path() {
                            Some(p) => {
                                let q = is_file && queue.iter().any(|x| x == p);
                                let a = is_file
                                    && active_path.as_deref() == Some(p.as_path());
                                (i, q, a)
                            }
                            None => {
                                // Entfernter Eintrag: Aktiv-Markierung über rel_path.
                                let a = is_file
                                    && active_rel.is_some()
                                    && r.entry.rel_path() == active_rel.as_deref();
                                (i, false, a)
                            }
                        }
                    })
                })
                .collect()
        };
        let playing = self.playing;
        for (i, q, a) in states {
            self.entries.send(i, FsInput::SetQueued(q));
            self.entries.send(i, FsInput::SetActive { active: a, playing });
        }
        // Play-Zeile eines offenen Detail-Dialogs mit dem Wiedergabestand abgleichen.
        self.refresh_ctx_play();
        // Play/Pause-Icons der Podcast-Beiträge (und die Detail-„Abspielen"-Zeile).
        self.refresh_episode_icons();
    }

    /// Zugangsdaten der aktuell aktiven WebDAV-Quelle (falls eine aktiv ist).
    pub(crate) fn active_webdav_creds(&self) -> Option<Creds> {
        let ActiveSource::Source(id) = self.active_source else {
            return None;
        };
        let s = self.sources.iter().find(|s| s.id == id)?;
        if s.kind != "webdav" {
            return None;
        }
        Creds::from_source(s)
    }

    /// Lokaler Cache-Pfad einer entfernten Datei der aktiven Quelle (oder `None`).
    pub(crate) fn remote_cache_path(&self, rel: &str) -> Option<PathBuf> {
        let ActiveSource::Source(id) = self.active_source else {
            return None;
        };
        Some(webdav::cache_path(id, rel))
    }

    /// Eine entfernte Datei antippen: erneutes Antippen des laufenden Titels
    /// schaltet Pause/Weiter um; sonst wird die Ordner-Reihe als entfernte Queue
    /// gesetzt und ab dem gewählten Titel abgespielt.
    pub(crate) fn activate_remote(&mut self, rel: &str) {
        let is_active = self.playing_remote
            && self
                .remote_queue
                .get(self.remote_pos)
                .is_some_and(|t| t.rel_path == rel);
        if is_active {
            if self.playing {
                self.save_resume();
                self.player.pause();
            } else {
                self.player.resume();
            }
            self.playing = !self.playing;
            self.mpris.set_playing(self.playing);
            self.refresh_queue_icons();
            return;
        }
        // Entfernte Reihe aus den sichtbaren Dateizeilen aufbauen (Ordnerfolge).
        let mut queue = Vec::new();
        let mut start = 0;
        {
            let guard = self.entries.guard();
            for i in 0..guard.len() {
                if let Some(row) = guard.get(i) {
                    if let FsEntry::RemoteFile { rel_path, .. } = &row.entry {
                        if rel_path == rel {
                            start = queue.len();
                        }
                        queue.push(RemoteTrack {
                            rel_path: rel_path.clone(),
                            title: row.entry.display_title(),
                        });
                    }
                }
            }
        }
        if queue.is_empty() {
            return;
        }
        self.remote_queue = queue;
        self.remote_pos = start;
        self.play_remote_current();
    }

    /// Spielt den aktuellen Titel der entfernten Reihe – lokal (falls bereits
    /// heruntergeladen) oder gestreamt. Eigenständig wie Podcast/Sender; die
    /// lokale `PathBuf`-Warteschlange bleibt dabei leer.
    pub(crate) fn play_remote_current(&mut self) {
        let Some(creds) = self.active_webdav_creds() else {
            return;
        };
        let Some(track) = self.remote_queue.get(self.remote_pos).cloned() else {
            return;
        };
        self.save_resume();
        self.save_episode_progress();
        self.finalize_play_session(false);
        let cached = self.remote_cache_path(&track.rel_path);
        let result = match &cached {
            Some(p) if p.exists() => self.player.play_file(&p.to_string_lossy(), 0),
            _ => self.player.play_uri(&webdav::stream_uri(&creds, &track.rel_path), 0),
        };
        match result {
            Ok(()) => {
                self.now_playing = Some(track.title.clone());
                self.playing = true;
                self.playing_path = None;
                self.playing_episode_url = None;
                self.playing_stream = None;
                self.playing_remote = true;
                self.stop_recorder();
                self.queue.clear();
                self.queue_pos = 0;
                self.position_ms = 0;
                self.track_duration_ms = 0;
                *self.close_resume.borrow_mut() = None;
                self.mpris.set_metadata(0, &track.title, None, None, None, None);
                self.mpris.set_playing(true);
                self.set_chapters(Vec::new());
                self.refresh_queue_icons();
            }
            Err(e) => {
                tracing::error!("Failed to play remote file: {e}");
                self.toast(&crate::i18n::gettext("Could not play this file"));
            }
        }
    }

    /// Nächster Titel der entfernten Reihe (für Next-Taste und EOS-Weiterschalten).
    pub(crate) fn remote_next(&mut self) {
        if self.remote_pos + 1 < self.remote_queue.len() {
            self.remote_pos += 1;
            self.play_remote_current();
        } else if self.repeat && !self.remote_queue.is_empty() {
            self.remote_pos = 0;
            self.play_remote_current();
        } else {
            // Ende der Reihe – Wiedergabe stoppen (wie am Episodenende).
            self.player.stop();
            self.playing = false;
            self.mpris.set_playing(false);
            self.refresh_queue_icons();
        }
    }

    /// Voriger Titel der entfernten Reihe.
    pub(crate) fn remote_prev(&mut self) {
        if self.remote_pos > 0 {
            self.remote_pos -= 1;
            self.play_remote_current();
        }
    }

    /// Baut die Zufalls-Reihenfolge neu auf (Fisher-Yates), mit dem aktuell
    /// laufenden Titel an erster Stelle. So spielt jeder Titel der Queue genau
    /// einmal, in zufälliger Folge.
    pub(crate) fn rebuild_shuffle_order(&mut self) {
        let len = self.queue.len();
        let mut order: Vec<usize> = (0..len).collect();
        for i in (1..len).rev() {
            let j = gtk::glib::random_int_range(0, (i + 1) as i32) as usize;
            order.swap(i, j);
        }
        // Laufenden Titel nach vorn, damit er nicht sofort übersprungen wird.
        if let Some(p) = order.iter().position(|&x| x == self.queue_pos) {
            order.swap(0, p);
        }
        self.shuffle_order = order;
        self.shuffle_idx = 0;
    }

    /// Nächster Titel: bei Zufall der nächste der Zufalls-Reihenfolge, sonst der
    /// folgende. Am Ende (alle gespielt) wird gestoppt.
    pub(crate) fn play_next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let len = self.queue.len();
        let next = if self.shuffle {
            // Neu mischen, wenn die Queue sich geänderte hat oder der laufende
            // Titel nicht (mehr) der erwartete der Reihenfolge ist (z. B. nach
            // manueller Auswahl) – dann ab dem aktuellen Titel weiterwürfeln.
            if self.shuffle_order.len() != len
                || self.shuffle_order.get(self.shuffle_idx) != Some(&self.queue_pos)
            {
                self.rebuild_shuffle_order();
            }
            if self.shuffle_idx + 1 < self.shuffle_order.len() {
                self.shuffle_idx += 1;
                Some(self.shuffle_order[self.shuffle_idx])
            } else {
                None
            }
        } else if self.queue_pos + 1 < len {
            Some(self.queue_pos + 1)
        } else {
            None
        };
        match next {
            Some(n) => {
                self.queue_pos = n;
                self.play_current();
            }
            None if self.repeat && !self.queue.is_empty() => {
                // Wiederholen: am Ende von vorn beginnen (Einzeltitel ebenso, da
                // die Queue dann nur einen Eintrag hat). Bei Zufall neu mischen.
                if self.shuffle {
                    self.rebuild_shuffle_order();
                    self.queue_pos = self.shuffle_order.first().copied().unwrap_or(0);
                } else {
                    self.queue_pos = 0;
                }
                self.play_current();
            }
            None => {
                // Ende der Wiedergabe: anhalten und an den Anfang der Queue
                // zurückspulen, damit die Play-Taste wieder auf „Play" steht und
                // ein erneuter Druck von vorn beginnt (siehe TogglePlay).
                self.save_resume();
                // Laufende Sitzung abschließen (No-op, falls bereits per EOS getan).
                self.finalize_play_session(false);
                self.player.stop();
                self.playing = false;
                self.playing_path = None;
                self.queue_pos = 0;
                self.position_ms = 0;
                self.track_duration_ms = 0;
                self.shuffle_order.clear();
                self.shuffle_idx = 0;
                *self.close_resume.borrow_mut() = None;
                self.mpris.set_stopped();
                self.refresh_queue_icons();
                self.save_queue();
            }
        }
    }

    /// Zurück-Taste: einmaliges Drücken startet den laufenden Titel von vorn,
    /// ein zweites Drücken **innerhalb einer Sekunde** springt zum zuvor
    /// gespielten Titel (History).
    pub(crate) fn play_prev(&mut self) {
        let now = std::time::Instant::now();
        let double = self
            .last_prev
            .is_some_and(|t| now.duration_since(t) <= std::time::Duration::from_secs(1));
        self.last_prev = Some(now);

        if double {
            if let Some(prev) = self.play_history.pop() {
                // Vorheriges Lied: bevorzugt an seiner Queue-Position spielen,
                // sonst direkt den Pfad (ohne erneuten History-Eintrag).
                self.skip_history_push = true;
                if let Some(pos) = self.queue.iter().position(|p| *p == prev) {
                    self.queue_pos = pos;
                    self.play_current();
                } else {
                    self.queue = vec![prev];
                    self.queue_pos = 0;
                    self.play_current();
                }
                return;
            }
            // Keine History → sequentiell ein Lied zurück.
            if !self.queue.is_empty() && self.queue_pos > 0 {
                self.skip_history_push = true;
                self.queue_pos -= 1;
                self.play_current();
            }
            return;
        }

        // Erstes Drücken: Wurde gerade erst ein neuer Kontext gestartet (Titel
        // läuft erst < 5 s) und liegt ein verdrängter Kontext auf dem Stapel, so
        // diesen **samt Playlist** wiederherstellen und das Lied weiterhören
        // (Resume aus der DB). „Zurück" direkt nach einem versehentlich
        // gestarteten Lied bringt damit zum vorherigen zurück.
        if !self.nav_stack.is_empty() && self.player.position_ms().unwrap_or(0) < 5000 {
            if let Some((q, pos)) = self.nav_stack.pop() {
                self.skip_history_push = true;
                self.queue = q;
                self.queue_pos = pos.min(self.queue.len().saturating_sub(1));
                self.play_current();
                self.refresh_queue_icons();
                return;
            }
        }

        // Sonst: laufenden Titel von vorn.
        if self.playing_path.is_some() {
            self.skip_history_push = true;
            self.play_current();
        }
    }

    /// Spielt den aktuellen Eintrag der Warteschlange ab.
    /// Anzeigename eines Titels für die Leiste: „Interpret - Titel" aus den Tags,
    /// notfalls der Dateiname.
    /// Startet die Wiedergabe eines Titel-Pfads. Lokale Pfade laufen über
    /// `play_file`; **entfernte** Titel (synthetischer Pfad `nc:<id>:<rel>`) werden
    /// aus dem lokalen Cache gespielt oder direkt von der Nextcloud gestreamt.
    pub(crate) fn start_track_playback(&self, path_str: &str, resume_ms: i64) -> anyhow::Result<()> {
        if let Some((sid, rel)) = crate::core::webdav::parse_nc_path(path_str) {
            let cache = crate::core::webdav::cache_path(sid, &rel);
            if cache.exists() {
                return self.player.play_file(&cache.to_string_lossy(), resume_ms);
            }
            if let Some(creds) = self
                .sources
                .iter()
                .find(|s| s.id == sid)
                .and_then(crate::core::webdav::Creds::from_source)
            {
                return self
                    .player
                    .play_uri(&crate::core::webdav::stream_uri(&creds, &rel), resume_ms);
            }
            return Err(anyhow::anyhow!("Nextcloud source unavailable"));
        }
        self.player.play_file(path_str, resume_ms)
    }

    /// Anzeigename eines Titels für Leiste/Warteschlange: bevorzugt aus der
    /// Datenbank (funktioniert auch für entfernte Titel), sonst aus der Datei.
    pub(crate) fn display_name(&self, path: &std::path::Path) -> String {
        let path_str = path.to_string_lossy();
        if let Ok(Some(t)) = self.library.track_by_path(&path_str) {
            let title = if t.title.trim().is_empty() {
                path.file_stem().and_then(|n| n.to_str()).unwrap_or("").to_string()
            } else {
                t.title
            };
            return match t.artist {
                Some(a) if !a.trim().is_empty() => format!("{a} - {title}"),
                _ => title,
            };
        }
        Self::track_display_name(path)
    }

    pub(crate) fn track_display_name(path: &std::path::Path) -> String {
        let stem = || {
            path.file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        };
        match scanner::read_track(path) {
            Ok(t) => {
                let title = if t.title.trim().is_empty() {
                    stem()
                } else {
                    t.title
                };
                match t.artist {
                    Some(a) if !a.trim().is_empty() => format!("{a} - {title}"),
                    _ => title,
                }
            }
            Err(_) => stem(),
        }
    }

    /// Für **alle** Titel wird eine Resume-Position geführt: beim nächsten Start
    /// läuft der Titel dort weiter, wo er gestoppt wurde. Die `guarded_resume`-
    /// Wächter sorgen dafür, dass ein quasi fertiger oder gerade erst begonnener
    /// Titel wieder von vorn startet.
    pub(crate) fn should_resume(&self, _t: &Track) -> bool {
        true
    }

    /// Sichert die aktuelle Warteschlange (Pfade + Position) für die
    /// Wiederherstellung nach einem Neustart der App.
    pub(crate) fn save_queue(&self) {
        let paths = self
            .queue
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = self.library.set_setting("queue_paths", &paths);
        let _ = self
            .library
            .set_setting("queue_pos", &self.queue_pos.to_string());
    }

    /// Sichert die aktuelle Wiedergabeposition des geladenen Titels als
    /// Resume-Punkt. Nahe Anfang oder Ende wird auf 0 zurückgesetzt, damit ein
    /// quasi fertiger Titel beim nächsten Mal von vorn beginnt.
    pub(crate) fn save_resume(&self) {
        let Some(path) = self.playing_path.clone() else {
            return;
        };
        let path_str = path.to_string_lossy();
        let Some(track) = self.library.track_by_path(&path_str).ok().flatten() else {
            return;
        };
        if !self.should_resume(&track) {
            return;
        }
        let Some(pos) = self.player.position_ms() else {
            return;
        };
        let dur = self.player.duration_ms().or(track.duration_ms).unwrap_or(0);
        let _ = self
            .library
            .set_resume_path(&path_str, guarded_resume(pos, dur));
    }

    /// Sichert die Wiedergabeposition der laufenden Podcast-Episode (Resume,
    /// anhand der Audio-URL). Nah am Anfang/Ende wird auf 0 gesetzt (gilt als
    /// neu bzw. fertig). No-op, wenn gerade keine Episode läuft.
    pub(crate) fn save_episode_progress(&self) {
        let Some(url) = self.playing_episode_url.clone() else {
            return;
        };
        let Some(pos) = self.player.position_ms() else {
            return;
        };
        let dur = self.player.duration_ms().unwrap_or(self.track_duration_ms);
        let _ = self
            .library
            .set_episode_progress(&url, guarded_resume(pos, dur));
    }

    /// Schließt die laufende Hör-Sitzung ab und schreibt sie als ein
    /// `play_event` in die Statistik. `completed` = bis zum Ende (EOS) gehört.
    /// Ohne Sitzung passiert nichts (idempotent).
    pub(crate) fn finalize_play_session(&mut self, completed: bool) {
        if let Some(s) = self.play_session.take() {
            let dur = if s.duration_ms > 0 {
                s.duration_ms
            } else {
                self.track_duration_ms
            };
            let _ = self.library.log_play(
                &s.path.to_string_lossy(),
                s.started_at,
                s.played_ms,
                dur,
                completed,
                None, // Quelle (queue/album/…) bleibt v1 ungenutzt, Spalte reserviert.
            );
        }
        *self.close_session.borrow_mut() = None;
    }

    pub(crate) fn play_current(&mut self) {
        // Position des bisher laufenden Titels sichern, bevor ein neuer geladen wird.
        self.save_resume();
        // Lief zuvor eine Podcast-Episode, deren Resume-Position sichern.
        self.save_episode_progress();
        // Bisherige Hör-Sitzung als Wechsel/Skip abschließen (kam der Aufruf von
        // einem EOS, ist sie bereits abgeschlossen → No-op).
        self.finalize_play_session(false);
        let Some(path) = self.queue.get(self.queue_pos).cloned() else {
            return;
        };
        // Kontext-Wechsel erkennen: ersetzt eine neue Auswahl die laufende
        // Warteschlange, den alten Kontext (Queue + Position) auf den Zurück-
        // Stapel legen – so lässt sich „voriges Lied **inkl. Playlist**
        // weiterhören". Beim Zurückspringen selbst nicht erneut stapeln.
        if !self.skip_history_push {
            if let Some((pq, pp)) = self.prev_ctx.clone() {
                if !pq.is_empty() && pq != self.queue {
                    self.nav_stack.push((pq, pp));
                    if self.nav_stack.len() > 50 {
                        self.nav_stack.remove(0);
                    }
                }
            }
        }
        // History pflegen: bisher laufenden Titel merken (für „vorheriges Lied").
        // Beim Zurückspringen aus der History selbst nicht erneut eintragen.
        if self.skip_history_push {
            self.skip_history_push = false;
        } else if let Some(prev) = self.playing_path.clone() {
            if prev != path {
                self.play_history.push(prev);
                if self.play_history.len() > 200 {
                    self.play_history.remove(0);
                }
            }
        }
        let path_str = path.to_string_lossy().to_string();
        // Gespeicherte Resume-Position (für alle Titel; s. should_resume).
        let track = self.library.track_by_path(&path_str).ok().flatten();
        let resume_ms = match &track {
            Some(t) if self.should_resume(t) => t.resume_ms,
            _ => 0,
        };
        match self.start_track_playback(&path_str, resume_ms) {
            Ok(()) => {
                self.playing_path = Some(path.clone());
                // Es läuft wieder Musik – keine Podcast-Episode/kein Sender/keine
                // entfernte Datei mehr aktiv.
                self.playing_episode_url = None;
                self.playing_stream = None;
                self.playing_remote = false;
                self.stop_recorder();
                self.now_playing = Some(self.display_name(&path));
                self.playing = true;
                // Aktiven Ausgang (kann sich geändert haben) auffrischen.
                self.active_output = crate::core::output::default_output().unwrap_or_default();
                self.apply_current_eq();
                // Sperrbildschirm/Medientasten über den neuen Titel informieren.
                self.update_mpris_metadata(&path, track.as_ref());
                self.mpris.set_playing(true);
                let start = self.player.position_ms().unwrap_or(resume_ms.max(0));
                self.mpris.set_position(start);
                self.mpris.seeked(start);
                // Seekleiste auf den neuen Titel setzen (Dauer verfeinert der Tick).
                self.position_ms = start;
                self.track_duration_ms = self
                    .player
                    .duration_ms()
                    .or_else(|| track.as_ref().and_then(|t| t.duration_ms))
                    .unwrap_or(0);
                // Schnappschuss für das Sichern beim Schließen (nur Resume-Titel).
                let resumable = matches!(&track, Some(t) if self.should_resume(t));
                *self.close_resume.borrow_mut() = resumable
                    .then(|| (path_str.clone(), start, self.track_duration_ms));
                // Neue Hör-Sitzung für die Statistik starten.
                let now = crate::ui::app::unix_now();
                self.play_session = Some(PlaySession {
                    path: path.clone(),
                    started_at: now,
                    played_ms: 0,
                    duration_ms: self.track_duration_ms,
                });
                *self.close_session.borrow_mut() =
                    Some((path_str.clone(), now, 0, self.track_duration_ms));
                // Play-/Queue-Markierungen in der Liste an den neuen Titel anpassen.
                self.refresh_queue_icons();
                // Warteschlange + Position für den nächsten Start sichern.
                self.save_queue();
                // Aktuellen Kontext merken (Erkennung künftiger Queue-Wechsel).
                self.prev_ctx = Some((self.queue.clone(), self.queue_pos));
                // Titel haben keine Kapitel → Marken/Hover-Liste säubern.
                self.set_chapters(Vec::new());
                // Fehlen brauchbare Tags (Interpret/Album), den Titel im Hintergrund
                // per Fingerprint erkennen lassen – statt eines Massen-Laufs nur das,
                // was tatsächlich gespielt wird. Die eigentlichen Gucklöcher (Key,
                // fpcalc, Netz, Versuchsgrenze) prüft fetch_focus_track.
                let needs_id = track.as_ref().map_or(true, |t| {
                    t.artist.as_deref().unwrap_or("").trim().is_empty()
                        || t.album.as_deref().unwrap_or("").trim().is_empty()
                });
                if needs_id && self.acoustid_key.as_deref().is_some_and(|k| !k.is_empty()) {
                    let _ = self.input.send(Msg::FingerprintCurrent(path.clone()));
                }
            }
            Err(e) => tracing::error!("Playback failed: {e}"),
        }
    }

    /// Schickt die Metadaten des laufenden Titels an den MPRIS-Dienst
    /// (Sperrbildschirm). Cover wird – falls vorhanden – best effort ergänzt.
    pub(crate) fn update_mpris_metadata(&self, path: &std::path::Path, track: Option<&Track>) {
        let (title, artist, album, length) = match track {
            Some(t) => (
                t.title.clone(),
                t.artist.clone(),
                t.album.clone(),
                t.duration_ms,
            ),
            None => (Self::track_display_name(path), None, None, None),
        };
        let art = album
            .as_deref()
            .and_then(|al| self.library.album_cover(al).ok().flatten());
        self.mpris.set_metadata(
            self.queue_pos,
            &title,
            artist.as_deref(),
            album.as_deref(),
            length,
            art.as_deref(),
        );
    }

    /// Löst den Equalizer für den laufenden Titel + aktiven Ausgang auf
    /// (Titel→Album→Interpret→Global, dann Standard-Ausgang) und setzt ihn live.
    /// Ohne Festlegung: neutral (alle Bänder 0).
    pub(crate) fn apply_current_eq(&self) {
        let Some(path) = self.queue.get(self.queue_pos) else {
            return;
        };
        let (artist, album) = match scanner::read_track(path) {
            Ok(t) => (t.artist, t.album),
            Err(_) => (None, None),
        };
        let bands = self
            .library
            .resolve_eq(
                &self.active_output,
                artist.as_deref(),
                album.as_deref(),
                &path.to_string_lossy(),
            )
            .unwrap_or([0.0; 10]);
        self.player.set_eq_bands(&bands);
    }

    /// Spielt einen Pfad ab (Ordner rekursiv bzw. Einzeldatei) als **eine**
    /// Warteschlange. Bei Mehr-CD-Inhalten (z. B. Live-Konzerten) werden die CDs
    /// zusammen abgespielt: zuerst CD1, dann CD2 … – sortiert nach Unterordner
    /// (CD-Ordner), dann Disc- und Tracknummer aus den Tags, sonst Dateiname.
    pub(crate) fn play_path(&mut self, path: &str, is_dir: bool) {
        let p = PathBuf::from(path);
        let files = if is_dir {
            let mut fs = scanner::collect_audio_files(&p);
            // Wie die Anzeige (`folder_tracks_ordered`): **natürliche** Pfad-
            // Sortierung, damit Wiedergabe- und Anzeigereihenfolge übereinstimmen
            // (CD-Ordner + Dateinamen geben die Reihenfolge vor, robust gegen
            // falsche/fehlende Disc-/Track-Tags).
            fs.sort_by_cached_key(|f| {
                crate::ui::app_views::natural_key(&f.to_string_lossy())
            });
            fs
        } else {
            vec![p]
        };
        if !files.is_empty() {
            self.queue = files;
            self.queue_pos = 0;
            self.play_current();
            self.refresh_queue_icons();
        }
    }
}
