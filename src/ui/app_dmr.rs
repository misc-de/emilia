//! "Receive from the network": Emilia as a DLNA/UPnP speaker and as an
//! AirPlay 1 receiver. Starts and stops both (`crate::core::cast::dmr`,
//! `crate::core::cast::airplay`) with their settings, turns the
//! controller's commands into the app's normal playback paths (a loaded URL
//! plays like a podcast episode; play/pause/seek/volume go the way the
//! desktop's MPRIS commands go) and reports the playback state back after
//! every message.

use relm4::ComponentSender;

use crate::core::cast::airplay::{AirPlayEvent, AirPlayHandle};
use crate::core::cast::dmr::{DmrCommand, DmrHandle, DmrStatus};
use crate::ui::app::{App, Msg};

impl App {
    /// Starts or stops the DLNA renderer and the AirPlay receiver to match
    /// their settings.
    pub(crate) fn apply_dmr(&mut self, sender: &ComponentSender<Self>) {
        let name = self
            .library
            .get_setting("sync_device_name")
            .ok()
            .flatten()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(crate::core::sync::default_device_name);
        let shown = format!("Emilia ({name})");
        let seed = format!(
            "{name}|{}",
            self.files.music_dir.clone().unwrap_or_default()
        );
        if !self.dmr.enabled {
            self.dmr.handle = None;
        } else if self.dmr.handle.is_none() {
            match DmrHandle::start(&shown, &seed) {
                Ok((handle, rx)) => {
                    let input = sender.input_sender().clone();
                    relm4::gtk::glib::spawn_future_local(async move {
                        while let Ok(cmd) = rx.recv().await {
                            input.emit(Msg::Dmr(cmd));
                        }
                    });
                    self.dmr.handle = Some(handle);
                    self.publish_dmr();
                }
                Err(e) => tracing::warn!("DLNA renderer could not start: {e:#}"),
            }
        }
        if !self.dmr.airplay_enabled {
            self.dmr.airplay = None;
            if self.dmr.airplay_now.take().is_some() {
                self.sync_cast_ui();
            }
        } else if self.dmr.airplay.is_none() {
            match AirPlayHandle::start(&shown, &seed) {
                Ok((handle, rx)) => {
                    let input = sender.input_sender().clone();
                    relm4::gtk::glib::spawn_future_local(async move {
                        while let Ok(ev) = rx.recv().await {
                            input.emit(Msg::AirPlay(ev));
                        }
                    });
                    self.dmr.airplay = Some(handle);
                }
                Err(e) => tracing::warn!("AirPlay receiver could not start: {e:#}"),
            }
        }
    }

    /// An AirPlay sender's stream: our own playback steps aside while it
    /// plays, and the cast picker shows what arrives.
    pub(crate) fn on_airplay(&mut self, ev: AirPlayEvent) {
        match ev {
            AirPlayEvent::Started => {
                if self.mini.playing {
                    self.on_toggle_play();
                }
                self.dmr.airplay_now.get_or_insert_with(String::new);
            }
            AirPlayEvent::Meta { title, artist, .. } => {
                self.dmr.airplay_now = Some(match artist {
                    Some(a) => format!("{a} – {title}"),
                    None => title,
                });
            }
            AirPlayEvent::Stopped => self.dmr.airplay_now = None,
        }
        self.sync_cast_ui();
    }

    pub(crate) fn on_dmr(&mut self, cmd: DmrCommand) {
        match cmd {
            DmrCommand::Load { uri, title, artist } => {
                let shown = match artist {
                    Some(a) => format!("{a} – {title}"),
                    None => title,
                };
                self.play_episode_at(&uri, &shown, 0);
            }
            DmrCommand::Play => {
                if !self.mini.playing {
                    self.on_toggle_play();
                }
            }
            // Stop keeps the item (like the desktop's Stop for anything with
            // a position): the controller usually follows with a new URI.
            DmrCommand::Pause | DmrCommand::Stop => {
                if self.mini.playing {
                    self.on_toggle_play();
                }
            }
            DmrCommand::Seek(ms) => self.seek_from_desktop(ms),
            DmrCommand::Volume(vol) => {
                self.player.set_master_volume(vol);
                self.mpris.set_volume(self.player.master_volume());
            }
        }
        self.publish_dmr();
    }

    /// Hands the current playback to the renderer (cheap; called after every
    /// message).
    pub(crate) fn publish_dmr(&self) {
        let Some(h) = &self.dmr.handle else { return };
        let uri = h.current_uri();
        let ours =
            !uri.is_empty() && self.podcasts.playing_episode_url.as_deref() == Some(uri.as_str());
        h.publish(DmrStatus {
            ours,
            playing: self.mini.playing,
            loaded: self.mini.now_playing.is_some(),
            pos_ms: self.mini.position_ms,
            dur_ms: Some(self.mini.track_duration_ms).filter(|&d| d > 0),
            volume: self.player.master_volume(),
        });
    }
}
