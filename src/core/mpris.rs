//! MPRIS-Bridge: Steuerung über Sperrbildschirm und Medientasten
//! (`org.mpris.MediaPlayer2`) via D-Bus.
//!
//! Alles läuft auf dem glib-Main-Loop (die `mpris-server`-`Player`-Tasks werden
//! mit `spawn_future_local` gestartet), darum kommen die Desktop-Befehle direkt
//! auf dem UI-Thread an – kein Thread-Bridging nötig. Ist kein D-Bus erreichbar
//! (z. B. headless), schlägt der Aufbau still fehl und alle Aufrufe sind No-Ops.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use mpris_server::{Metadata, PlaybackStatus, Player, Time, TrackId};

/// Befehl vom Desktop an die App. Wird auf dem Main-Thread ausgeliefert.
#[derive(Debug, Clone, Copy)]
pub enum MprisCommand {
    PlayPause,
    Play,
    Pause,
    Next,
    Prev,
    Stop,
    Raise,
    /// Relativer Sprung um Mikrosekunden (kann negativ sein).
    SeekBy(i64),
    /// Sprung an eine absolute Position in Mikrosekunden.
    SetPosition(i64),
}

/// Der `Player` wird erst asynchron aufgebaut; bis dahin (oder wenn kein D-Bus
/// vorhanden ist) bleibt der Platz leer und alle Aufrufe sind wirkungslos.
type Slot = Rc<RefCell<Option<Rc<Player>>>>;

/// Griff auf den laufenden MPRIS-Dienst zum Aktualisieren des Zustands.
#[derive(Clone)]
pub struct Mpris {
    player: Slot,
}

impl Mpris {
    /// Startet den MPRIS-Dienst auf dem glib-Main-Loop. `on_cmd` wird bei jedem
    /// Desktop-Befehl (Play/Pause/Next/…) auf dem Main-Thread aufgerufen.
    pub fn start<F>(on_cmd: F) -> Self
    where
        F: Fn(MprisCommand) + 'static,
    {
        let slot: Slot = Rc::new(RefCell::new(None));
        let on_cmd: Rc<dyn Fn(MprisCommand)> = Rc::new(on_cmd);
        // Eindeutiger Bus-Name je Prozess: im Dev-Modus laufen mehrere Instanzen
        // (NON_UNIQUE), die sich sonst um denselben Namen streiten würden.
        let suffix = format!("Emilia.instance{}", std::process::id());

        let slot_for_task = slot.clone();
        glib::spawn_future_local(async move {
            let player = match Player::builder(&suffix)
                .identity("Emilia")
                .desktop_entry("de.cais.Emilia")
                .can_play(true)
                .can_pause(true)
                .can_go_next(true)
                .can_go_previous(true)
                .can_seek(true)
                .can_control(true)
                .can_raise(true)
                .build()
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("MPRIS unavailable: {e}");
                    return;
                }
            };

            macro_rules! forward {
                ($connect:ident, $cmd:expr) => {{
                    let cb = on_cmd.clone();
                    player.$connect(move |_| cb($cmd));
                }};
            }
            forward!(connect_play_pause, MprisCommand::PlayPause);
            forward!(connect_play, MprisCommand::Play);
            forward!(connect_pause, MprisCommand::Pause);
            forward!(connect_next, MprisCommand::Next);
            forward!(connect_previous, MprisCommand::Prev);
            forward!(connect_stop, MprisCommand::Stop);
            forward!(connect_raise, MprisCommand::Raise);
            {
                let cb = on_cmd.clone();
                player.connect_seek(move |_, offset: Time| {
                    cb(MprisCommand::SeekBy(offset.as_micros()))
                });
            }
            {
                let cb = on_cmd.clone();
                player.connect_set_position(move |_, _track: &TrackId, pos: Time| {
                    cb(MprisCommand::SetPosition(pos.as_micros()))
                });
            }

            // Eingehende Methodenaufrufe abarbeiten (läuft im Main-Loop weiter).
            glib::spawn_future_local(player.run());
            *slot_for_task.borrow_mut() = Some(Rc::new(player));
        });

        Mpris { player: slot }
    }

    /// Setzt den Wiedergabestatus (Playing/Paused).
    pub fn set_playing(&self, playing: bool) {
        self.set_status(if playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        });
    }

    pub fn set_stopped(&self) {
        self.set_status(PlaybackStatus::Stopped);
    }

    fn set_status(&self, status: PlaybackStatus) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        glib::spawn_future_local(async move {
            let _ = player.set_playback_status(status).await;
        });
    }

    /// Aktualisiert die Titel-Metadaten für den Sperrbildschirm. `index` dient
    /// als stabile (Sitzungs-)Track-ID; `length_ms`/`art_path` sind optional.
    pub fn set_metadata(
        &self,
        index: usize,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
        length_ms: Option<i64>,
        art_path: Option<&str>,
    ) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        let mut b = Metadata::builder().title(title);
        if let Ok(tid) = TrackId::try_from(format!("/de/cais/Emilia/track/{index}")) {
            b = b.trackid(tid);
        }
        if let Some(a) = artist.filter(|s| !s.is_empty()) {
            b = b.artist([a]);
        }
        if let Some(al) = album.filter(|s| !s.is_empty()) {
            b = b.album(al);
        }
        if let Some(ms) = length_ms.filter(|&m| m > 0) {
            b = b.length(Time::from_millis(ms));
        }
        if let Some(uri) = art_path
            .filter(|s| !s.is_empty())
            .and_then(|p| glib::filename_to_uri(p, None).ok())
        {
            b = b.art_url(uri.to_string());
        }
        let metadata = b.build();
        glib::spawn_future_local(async move {
            let _ = player.set_metadata(metadata).await;
        });
    }

    /// Setzt die aktuelle Position (für Lese-Abfragen der Clients). Synchron und
    /// günstig – für regelmäßige Updates gedacht.
    pub fn set_position(&self, pos_ms: i64) {
        if let Some(player) = self.player.borrow().as_ref() {
            player.set_position(Time::from_millis(pos_ms.max(0)));
        }
    }

    /// Meldet einen Positionssprung (Seeked-Signal) nach dem Spulen.
    pub fn seeked(&self, pos_ms: i64) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        glib::spawn_future_local(async move {
            let _ = player.seeked(Time::from_millis(pos_ms.max(0))).await;
        });
    }
}
