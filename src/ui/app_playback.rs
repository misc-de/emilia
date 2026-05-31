//! Wiedergabe: Warteschlange, Play/Pause/Next/Prev, Resume-Logik und der
//! laufende Equalizer. Aus app.rs herausgelöst – reine Umordnung, kein
//! Funktionswechsel; die Methoden bleiben inhärente `impl App`-Methoden.

use std::path::PathBuf;

use relm4::gtk;

use crate::core::scanner;
use crate::model::Track;
use crate::ui::app::{guarded_resume, App, RESUME_MIN_DURATION_MS};
use crate::ui::fs_row::FsInput;

impl App {
    /// Aktualisiert die Queue-Markierung aller sichtbaren Dateizeilen.
    pub(crate) fn refresh_queue_icons(&mut self) {
        let queue = self.queue.clone();
        // Aktuell laufender Titel (für die Play-Markierung).
        let active_path = self.queue.get(self.queue_pos).cloned();
        let states: Vec<(usize, bool, bool)> = {
            let guard = self.entries.guard();
            (0..guard.len())
                .filter_map(|i| {
                    guard.get(i).map(|r| {
                        let is_file = !r.entry.is_dir();
                        let q = is_file && queue.iter().any(|p| p == r.entry.path());
                        let a = is_file
                            && active_path.as_deref() == Some(r.entry.path().as_path());
                        (i, q, a)
                    })
                })
                .collect()
        };
        let playing = self.playing;
        for (i, q, a) in states {
            self.entries.send(i, FsInput::SetQueued(q));
            self.entries.send(i, FsInput::SetActive { active: a, playing });
        }
    }

    /// Nächster Titel: bei Zufall ein zufälliger, sonst der folgende.
    /// Am sequentiellen Ende wird gestoppt.
    pub(crate) fn play_next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let len = self.queue.len();
        let next = if self.shuffle {
            gtk::glib::random_int_range(0, len as i32) as usize
        } else if self.queue_pos + 1 < len {
            self.queue_pos + 1
        } else {
            self.save_resume();
            self.player.stop();
            self.playing = false;
            self.playing_path = None;
            self.position_ms = 0;
            self.track_duration_ms = 0;
            *self.close_resume.borrow_mut() = None;
            self.mpris.set_stopped();
            self.refresh_queue_icons();
            return;
        };
        self.queue_pos = next;
        self.play_current();
    }

    /// Vorheriger Titel (sequentiell).
    pub(crate) fn play_prev(&mut self) {
        if !self.queue.is_empty() && self.queue_pos > 0 {
            self.queue_pos -= 1;
            self.play_current();
        }
    }

    /// Spielt den aktuellen Eintrag der Warteschlange ab.
    /// Anzeigename eines Titels für die Leiste: „Interpret - Titel" aus den Tags,
    /// notfalls der Dateiname.
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

    /// Ob für diesen Titel eine Resume-Position geführt werden soll: bei langen
    /// Titeln (Hörspiele) immer, sonst nur, wenn er als Hörbuch oder Podcast
    /// eingestuft ist. Normale (kurze) Musiktitel starten stets von vorn.
    pub(crate) fn should_resume(&self, t: &Track) -> bool {
        if t.duration_ms.unwrap_or(0) >= RESUME_MIN_DURATION_MS {
            return true;
        }
        self.library
            .resolve_areas(t.artist.as_deref(), t.album.as_deref(), &t.path)
            .contains(&crate::core::category::Area::Audiobooks)
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

    pub(crate) fn play_current(&mut self) {
        // Position des bisher laufenden Titels sichern, bevor ein neuer geladen wird.
        self.save_resume();
        let Some(path) = self.queue.get(self.queue_pos).cloned() else {
            return;
        };
        let path_str = path.to_string_lossy().to_string();
        // Gespeicherte Resume-Position – nur für Lang-Inhalte (s. should_resume).
        let track = self.library.track_by_path(&path_str).ok().flatten();
        let resume_ms = match &track {
            Some(t) if self.should_resume(t) => t.resume_ms,
            _ => 0,
        };
        match self.player.play_file(&path_str, resume_ms) {
            Ok(()) => {
                self.playing_path = Some(path.clone());
                self.now_playing = Some(Self::track_display_name(&path));
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
                // Play-/Queue-Markierungen in der Liste an den neuen Titel anpassen.
                self.refresh_queue_icons();
            }
            Err(e) => tracing::error!("Wiedergabe fehlgeschlagen: {e}"),
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

    /// Spielt einen Pfad ab (Ordner rekursiv bzw. Einzeldatei) als neue Queue.
    pub(crate) fn play_path(&mut self, path: &str, is_dir: bool) {
        let p = PathBuf::from(path);
        let files = if is_dir {
            scanner::collect_audio_files(&p)
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
